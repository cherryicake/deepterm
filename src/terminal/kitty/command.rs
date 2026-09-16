//! Kitty 图形协议的命令解析（APC `ESC _ G <控制对>;<base64> ESC \`）。
//!
//! 结构对齐 ghostty `terminal/kitty/graphics_command.zig`：控制对是
//! `键=值` 用 `,` 分隔，第一个 `;` 之后是 base64 负载。键恒为单个 ASCII
//! 字母（多字符键整体忽略），值若是单个可打印字符则取其 ASCII 码，
//! 否则按无符号 32 位整数解析（仅 `z`/`H`/`V` 视为有符号）。

use std::collections::HashMap;

/// 单条命令的负载上限。
///
/// 对齐 ghostty `apc.zig::defaultMaxBytes(.kitty)` 的 65 MiB：图形负载是
/// 整幅图的 base64（terminal-browser 一类客户端会把大帧切成少数几条大分片，
/// 单条即可到数 MB），1 MiB 会把合法大图直接拒掉。
pub const MAX_COMMAND_BYTES: usize = 65 * 1024 * 1024;

/// 协议动作（`a=` 键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// `a=q` 查询终端能力。
    Query,
    /// `a=t` 传输图像（不显示）。
    Transmit,
    /// `a=T` 传输并显示。
    TransmitAndDisplay,
    /// `a=p` 显示已传输的图像。
    Display,
    /// `a=d` 删除。
    Delete,
    /// `a=f` 传输动画帧。
    TransmitAnimationFrame,
    /// `a=a` 动画控制（播放/停止）。
    ControlAnimation,
    /// `a=c` 合成动画帧。
    ComposeAnimation,
}

impl Action {
    fn from_byte(b: u8) -> Option<Self> {
        Some(match b {
            b'q' => Self::Query,
            b't' => Self::Transmit,
            b'T' => Self::TransmitAndDisplay,
            b'p' => Self::Display,
            b'd' => Self::Delete,
            b'f' => Self::TransmitAnimationFrame,
            b'a' => Self::ControlAnimation,
            b'c' => Self::ComposeAnimation,
            _ => return None,
        })
    }
}

/// 像素格式（`f=` 键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `f=24` 每像素 3 字节 RGB。
    Rgb,
    /// `f=32` / `f=0` 每像素 4 字节 RGBA。
    Rgba,
    /// `f=100` PNG（压缩）。
    Png,
}

/// 传输介质（`t=` 键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Medium {
    /// `t=d` 负载随命令直传（默认）。
    #[default]
    Direct,
    /// `t=f` 从文件读取。
    File,
    /// `t=t` 从临时文件读取。
    TemporaryFile,
    /// `t=s` 从 POSIX 共享内存读取。
    SharedMemory,
}

/// 负载压缩（`o=` 键）。
///
/// kitty 规范目前只定义了 `o=z`（zlib/DEFLATE 流）；其余值非法（对齐
/// ghostty `graphics_command.zig`：非 `'z'` 直接 `InvalidFormat`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    /// 未压缩。
    #[default]
    None,
    /// `o=z` zlib 流。
    Zlib,
}

impl Medium {
    /// 只有直传介质支持分片（对齐 kitty/ghostty 的行为：本地介质忽略 `m=`）。
    pub fn supports_chunking(self) -> bool {
        matches!(self, Self::Direct)
    }
}

/// 删除目标（`d=` 键）。大写字母表示同时释放不再被引用的图像数据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteTarget {
    /// `d=a` 全部；`d=A` 全部并释放数据。
    All,
    /// `d=i` 按图像 id；`d=I` 同并释放。
    ByImageId,
    /// `d=n` 最新图像；`d=N` 同并释放。
    ByImageNumber,
    /// `d=c` 与光标相交；`d=C` 同并释放。
    AtCursor,
    /// `d=f` 所有动画帧；`d=F` 同并释放。
    AnimationFrames,
    /// `d=p` 与指定单元格相交；`d=P` 同并释放。
    AtCell,
    /// `d=q` 与单元格及 z 层相交；`d=Q` 同并释放。
    AtCellWithZ,
    /// `d=r` 图像 id 区间；`d=R` 同并释放。
    ByImageIdRange,
    /// `d=x` 按列；`d=X` 同并释放。
    ByColumn,
    /// `d=y` 按行；`d=Y` 同并释放。
    ByRow,
    /// `d=z` 按 z 层；`d=Z` 同并释放。
    ByZIndex,
}

impl DeleteTarget {
    /// 由 `d=` 的字节解析出目标，第二项表示是否同时释放图像数据（大写形式）。
    pub fn from_byte(b: u8) -> Option<(Self, bool)> {
        Some(match b {
            b'a' => (Self::All, false),
            b'A' => (Self::All, true),
            b'i' => (Self::ByImageId, false),
            b'I' => (Self::ByImageId, true),
            b'n' => (Self::ByImageNumber, false),
            b'N' => (Self::ByImageNumber, true),
            b'c' => (Self::AtCursor, false),
            b'C' => (Self::AtCursor, true),
            b'f' => (Self::AnimationFrames, false),
            b'F' => (Self::AnimationFrames, true),
            b'p' => (Self::AtCell, false),
            b'P' => (Self::AtCell, true),
            b'q' => (Self::AtCellWithZ, false),
            b'Q' => (Self::AtCellWithZ, true),
            b'r' => (Self::ByImageIdRange, false),
            b'R' => (Self::ByImageIdRange, true),
            b'x' => (Self::ByColumn, false),
            b'X' => (Self::ByColumn, true),
            b'y' => (Self::ByRow, false),
            b'Y' => (Self::ByRow, true),
            b'z' => (Self::ByZIndex, false),
            b'Z' => (Self::ByZIndex, true),
            _ => return None,
        })
    }
}

/// 静默级别（`q=` 键）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quiet {
    /// `q=0` 回复所有命令（默认）。
    #[default]
    Never,
    /// `q=1` 只回复错误。
    OnError,
    /// `q>1` 全部静默。
    Always,
}

/// 解析后的控制对集合。
///
/// 键为单个 ASCII 字母；未知键在解析阶段即被丢弃（与 ghostty 一致）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ControlPairs {
    map: HashMap<u8, u32>,
}

impl ControlPairs {
    /// 取无符号值。
    pub fn u32(&self, key: u8) -> Option<u32> {
        self.map.get(&key).copied()
    }

    /// 取无符号值，并要求能容纳进 `u16`。
    pub fn u16(&self, key: u8) -> Option<u16> {
        self.u32(key).and_then(|v| u16::try_from(v).ok())
    }

    /// 取有符号值（仅 `z`/`H`/`V` 有语义）。
    pub fn i32(&self, key: u8) -> Option<i32> {
        self.u32(key).map(|v| v as i32)
    }

    /// 取单字符值（如 `t=d` 的 `d`）。
    pub fn byte(&self, key: u8) -> Option<u8> {
        self.u32(key).and_then(|v| u8::try_from(v).ok())
    }

    /// 是否存在该键。
    pub fn contains(&self, key: u8) -> bool {
        self.map.contains_key(&key)
    }

    fn put(&mut self, key: u8, value: u32) {
        // 只接受单个 ASCII 字母作为键。
        if key.is_ascii_alphabetic() {
            self.map.insert(key, value);
        }
    }
}

/// 一条完整的图形命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub action: Action,
    pub quiet: Quiet,
    /// 负载压缩方式（`o=` 键；PNG 负载不走此压缩）。
    pub compression: Compression,
    pub control: ControlPairs,
    /// 已 base64 解码的负载（直传介质）。
    pub data: Vec<u8>,
    /// **未解码**的 base64 文本（直传介质）。
    ///
    /// 分片传输时必须以它为准：kitty 规定客户端「先整体 base64 编码、再把
    /// **编码后**的文本按 4 字节对齐切开」，接收方必须把各片的编码文本拼起来
    /// 再一次性解码。逐片解码只在每片都 4 对齐时等价，而规范只保证非末片
    /// 对齐——若某片恰好落在填充边界上，逐片解码会错位。因此分片路径累积
    /// 本字段，末片再解码（见 `LoadingImage`）。
    pub encoded: Vec<u8>,
}

/// 解析错误。错误码与 kitty 约定一致，用于生成响应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// 控制对格式非法（如 `a=1,b` 以键结尾）。
    InvalidFormat,
    /// 负载超限。
    TooLarge,
    /// 未知动作。
    UnknownAction,
}

impl ParseError {
    /// kitty 响应里的错误文本。
    pub fn message(self) -> &'static str {
        match self {
            Self::InvalidFormat => "EINVAL: invalid format",
            Self::TooLarge => "EINVAL: too large",
            Self::UnknownAction => "EINVAL: unknown action",
        }
    }
}

/// 解析器状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// 累积控制键名。
    Key,
    /// 累积控制值。
    Value,
    /// 键名非法（多字符），忽略到下一个分隔符。
    KeyIgnore,
    /// 值非法，忽略到下一个分隔符。
    ValueIgnore,
    /// 累积 base64 负载。
    Data,
}

/// 增量解析器。
///
/// 支持逐字节 `feed`，也支持在进入负载状态后整段追加（大图分片时的关键路径）。
#[derive(Debug)]
pub struct Parser {
    state: State,
    key_buf: Vec<u8>,
    value_buf: Vec<u8>,
    current_key: u8,
    control: ControlPairs,
    data: Vec<u8>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state: State::Key,
            key_buf: Vec::new(),
            value_buf: Vec::new(),
            current_key: 0,
            control: ControlPairs::default(),
            data: Vec::new(),
        }
    }

    /// 送入一个字节。
    pub fn feed(&mut self, byte: u8) -> Result<(), ParseError> {
        match self.state {
            State::Key => match byte {
                b'=' => {
                    if self.key_buf.len() != 1 {
                        // 控制键恒为单字符；多字符键整体忽略。
                        self.state = State::KeyIgnore;
                        self.key_buf.clear();
                    } else {
                        self.current_key = self.key_buf[0];
                        self.key_buf.clear();
                        self.value_buf.clear();
                        self.state = State::Value;
                    }
                }
                // 无控制对、直接进负载的形式（`\x1b_G;<data>`），kitty 视为合法。
                b';' => self.state = State::Data,
                b',' => self.key_buf.clear(),
                _ => self.key_buf.push(byte),
            },

            State::KeyIgnore => match byte {
                b'=' => self.state = State::ValueIgnore,
                b';' => self.state = State::Data,
                b',' => self.state = State::Key,
                _ => {}
            },

            State::Value => match byte {
                b',' => {
                    self.finish_value()?;
                    self.state = State::Key;
                }
                b';' => {
                    self.finish_value()?;
                    self.state = State::Data;
                }
                _ => self.value_buf.push(byte),
            },

            State::ValueIgnore => match byte {
                b',' => self.state = State::Key,
                b';' => self.state = State::Data,
                _ => {}
            },

            State::Data => {
                if self.data.len() >= MAX_COMMAND_BYTES {
                    return Err(ParseError::TooLarge);
                }
                self.data.push(byte);
            }
        }
        Ok(())
    }

    /// 送入一段字节。进入负载状态后整段追加，避免逐字节开销。
    pub fn feed_slice(&mut self, bytes: &[u8]) -> Result<(), ParseError> {
        let mut rest = bytes;
        while !rest.is_empty() {
            if self.state == State::Data {
                if self.data.len() + rest.len() > MAX_COMMAND_BYTES {
                    return Err(ParseError::TooLarge);
                }
                self.data.extend_from_slice(rest);
                return Ok(());
            }
            self.feed(rest[0])?;
            rest = &rest[1..];
        }
        Ok(())
    }

    /// 完成解析，产出命令。必须在所有字节送入之后调用。
    pub fn finish(mut self) -> Result<Command, ParseError> {
        // 只能以值或负载状态结束：以键结尾（如 `a=1,b`）非法。
        match self.state {
            State::Key | State::KeyIgnore => return Err(ParseError::InvalidFormat),
            // 值已累积完毕但尚未遇到 `,`/`;`，此处补写。
            State::Value => self.finish_value()?,
            State::ValueIgnore | State::Data => {}
        }

        let control = self.control;
        let action = match control.u32(b'a') {
            // `a` 缺省等价于 `a=t`。
            None => Action::Transmit,
            Some(v) => {
                let b = u8::try_from(v).map_err(|_| ParseError::InvalidFormat)?;
                Action::from_byte(b).ok_or(ParseError::UnknownAction)?
            }
        };

        let quiet = match control.u32(b'q') {
            None | Some(0) => Quiet::Never,
            Some(1) => Quiet::OnError,
            Some(_) => Quiet::Always,
        };

        let compression = match control.byte(b'o') {
            None => Compression::None,
            Some(b'z') => Compression::Zlib,
            Some(_) => return Err(ParseError::InvalidFormat),
        };

        let encoded = self.data.clone();
        let data = decode_base64(&self.data)?;

        Ok(Command {
            action,
            quiet,
            compression,
            control,
            data,
            encoded,
        })
    }

    /// 把已累积的值写入控制表。值是非法数字时报错（对齐 ghostty：解析
    /// 失败即整条命令 `EINVAL`，而不是把坏值当 0 悄悄接受）。
    fn finish_value(&mut self) -> Result<(), ParseError> {
        let value = parse_value(&self.value_buf, self.current_key)?;
        self.control.put(self.current_key, value);
        self.value_buf.clear();
        Ok(())
    }
}

/// 有符号键：只有这三个键允许负数（`z` 为层级 / 动画间隔，`H`/`V` 为相对偏移）。
fn is_signed_key(key: u8) -> bool {
    matches!(key, b'z' | b'H' | b'V')
}

/// 值解析：单个**非数字** ASCII 字符取其码值（用于 `t=d` 这类标志位），
/// 否则按十进制整数解析。
///
/// 判据必须排除数字：`i=1` 中的 `1` 是数值 1，不是字符 `'1'` 的码值 49。
/// 与 ghostty `graphics_command.zig::finishValue` 的 `c < '0' or c > '9'` 一致。
///
/// 负数按有符号解析后再按位转回 `u32` 存储，取用时经 [`ControlPairs::i32`]
/// 还原——这样 `z` 既能表达层级也能表达负值。
///
/// 非法数值（非 UTF-8、超范围、空值）返回 `Err`：ghostty 的 `finishValue`
/// 是 `!void`，`std.fmt.parseInt` 的失败会一路传出去变成 `EINVAL` 应答。
/// 静默按 0 处理会让客户端以为命令被接受（比如把 `i=abc` 当成图像 0）。
fn parse_value(buf: &[u8], key: u8) -> Result<u32, ParseError> {
    if let [single] = buf
        && !single.is_ascii_digit()
        && single.is_ascii()
    {
        return Ok(*single as u32);
    }
    let text = std::str::from_utf8(buf).map_err(|_| ParseError::InvalidFormat)?;
    if is_signed_key(key) {
        text.parse::<i32>()
            .map(|v| v as u32)
            .map_err(|_| ParseError::InvalidFormat)
    } else {
        text.parse::<u32>().map_err(|_| ParseError::InvalidFormat)
    }
}

/// base64 编码（标准字母表，带 `=` 填充）。
///
/// 是 [`decode_base64`] 的逆运算：协议要求负载与路径都以 base64 承载，测试与
/// 需要回写负载的调用方都得用它。
#[cfg_attr(not(test), allow(dead_code))]
pub fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);

    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }

    out
}

/// base64 解码。kitty 使用标准字母表，允许 `=` 填充。
///
/// 非法字符直接跳过而非报错——与 kitty 的宽容行为一致（部分客户端会插入换行）。
pub fn decode_base64(input: &[u8]) -> Result<Vec<u8>, ParseError> {
    const INVALID: u8 = 0xFF;
    let table = |c: u8| -> u8 {
        match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => INVALID,
        }
    };

    // 上界：每 4 个字符最多产出 3 字节。
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut bits: u32 = 0;

    for &c in input {
        if c == b'=' {
            continue;
        }
        let v = table(c);
        if v == INVALID {
            continue;
        }
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Command, ParseError> {
        let mut p = Parser::new();
        p.feed_slice(s.as_bytes())?;
        p.finish()
    }

    #[test]
    fn parses_simple_query() {
        let cmd = parse("a=q,i=1,f=32,s=1,v=1").unwrap();
        assert_eq!(cmd.action, Action::Query);
        assert_eq!(cmd.control.u32(b'i'), Some(1));
        assert_eq!(cmd.control.u32(b'f'), Some(32));
        assert_eq!(cmd.quiet, Quiet::Never);
    }

    #[test]
    fn compression_key() {
        // 缺省与 o=z。
        assert_eq!(
            parse("a=t,i=1,f=32,s=1,v=1").unwrap().compression,
            Compression::None
        );
        assert_eq!(
            parse("a=T,f=32,o=z,s=2,v=2").unwrap().compression,
            Compression::Zlib
        );
        // 非 'z' 的值：解析即报错（对齐 ghostty）。
        assert!(matches!(
            parse("a=t,f=32,o=q,s=1,v=1;AAAA"),
            Err(ParseError::InvalidFormat)
        ));
    }

    #[test]
    fn default_action_is_transmit() {
        let cmd = parse("i=1,f=32,s=1,v=1;AAAA").unwrap();
        assert_eq!(cmd.action, Action::Transmit);
    }

    #[test]
    fn parses_medium_and_delete_targets() {
        let cmd = parse("a=t,t=f,i=3,f=100").unwrap();
        assert_eq!(cmd.control.byte(b't'), Some(b'f'));

        let del = parse("a=d,d=A").unwrap();
        assert_eq!(del.action, Action::Delete);
        let (target, free) = DeleteTarget::from_byte(del.control.byte(b'd').unwrap()).unwrap();
        assert_eq!(target, DeleteTarget::All);
        assert!(free, "大写 d=A 应同时释放数据");
    }

    #[test]
    fn quiet_levels() {
        assert_eq!(parse("a=q,q=0").unwrap().quiet, Quiet::Never);
        assert_eq!(parse("a=q,q=1").unwrap().quiet, Quiet::OnError);
        assert_eq!(parse("a=q,q=2").unwrap().quiet, Quiet::Always);
    }

    #[test]
    fn unknown_key_is_ignored() {
        // 多字符键（`zz`）应整体丢弃，不影响其余控制对。
        let cmd = parse("a=t,zz=5,i=7").unwrap();
        assert_eq!(cmd.action, Action::Transmit);
        assert_eq!(cmd.control.u32(b'i'), Some(7));
    }

    #[test]
    fn value_ending_without_data_is_valid() {
        // 展示类命令可以没有负载（如 `a=p,i=1`）。
        let cmd = parse("a=p,i=1").unwrap();
        assert_eq!(cmd.action, Action::Display);
        assert!(cmd.data.is_empty());
    }

    #[test]
    fn trailing_key_is_invalid() {
        assert_eq!(parse("a=1,b"), Err(ParseError::InvalidFormat));
    }

    #[test]
    fn unknown_action_is_rejected() {
        assert_eq!(parse("a=Z"), Err(ParseError::UnknownAction));
    }

    #[test]
    fn non_numeric_value_is_invalid() {
        // ghostty `finishValue` 会 `try parseInt`：坏值让整条命令失败，
        // 而不是退化成 0 被当成"有效但为零"。
        assert_eq!(parse("a=t,i=abc,f=32"), Err(ParseError::InvalidFormat));
        // 允许多字符的标志位只对单字符生效；`t=ff` 是非法数字。
        assert_eq!(parse("a=t,t=ff"), Err(ParseError::InvalidFormat));
    }

    #[test]
    fn empty_value_is_invalid() {
        // `i=` 的缓冲区长度为 0，ghostty 同样会在 parseInt 上失败。
        assert_eq!(parse("a=t,i=,f=32"), Err(ParseError::InvalidFormat));
        assert_eq!(parse("a=t,i="), Err(ParseError::InvalidFormat));
    }

    #[test]
    fn overflowing_value_is_invalid() {
        // u32 上界 4294967295 可接受，再多一位即溢出。
        assert_eq!(
            parse("a=t,i=4294967295").unwrap().control.u32(b'i'),
            Some(u32::MAX)
        );
        assert_eq!(parse("a=t,i=4294967296"), Err(ParseError::InvalidFormat));
    }

    #[test]
    fn negative_value_only_allowed_on_signed_keys() {
        // `z` 是有符号键（层级 / 动画间隔），负数合法并保留位模式。
        let cmd = parse("a=t,i=1,z=-3").unwrap();
        assert_eq!(cmd.control.i32(b'z'), Some(-3));
        // 其余键负数非法。
        assert_eq!(parse("a=t,i=-1"), Err(ParseError::InvalidFormat));
    }

    #[test]
    fn single_non_digit_ascii_value_is_a_codepoint() {
        // 单字符标志位（`t=d`）取码值；数字字符仍按数值解析。
        assert_eq!(parse("a=t,t=d").unwrap().control.byte(b't'), Some(b'd'));
        assert_eq!(parse("a=t,i=7").unwrap().control.u32(b'i'), Some(7));
    }

    #[test]
    fn decodes_base64_payload() {
        // "AAAA" 解码为 3 个 0 字节。
        let cmd = parse("a=t,f=32,s=1,v=1;AAAA").unwrap();
        assert_eq!(cmd.data, vec![0, 0, 0]);
    }

    #[test]
    fn decodes_rgba_pixel() {
        // 单像素 RGBA (255,0,0,255)
        let cmd = parse("a=t,f=32,s=1,v=1;/wAA/w==").unwrap();
        assert_eq!(cmd.data, vec![255, 0, 0, 255]);
    }

    #[test]
    fn empty_data_after_semicolon_is_allowed() {
        let cmd = parse("a=q,i=1;").unwrap();
        assert_eq!(cmd.action, Action::Query);
        assert!(cmd.data.is_empty());
    }

    #[test]
    fn oversized_payload_is_rejected() {
        let mut p = Parser::new();
        let big = format!("a=t,f=32,s=1,v=1;{}", "A".repeat(MAX_COMMAND_BYTES + 4));
        assert_eq!(p.feed_slice(big.as_bytes()), Err(ParseError::TooLarge));
    }

    #[test]
    fn chunked_feeding_matches_bulk() {
        let bulk = parse("a=t,f=32,s=1,v=1;AAAA").unwrap();
        // 逐字节喂入应得到相同结果。
        let mut p = Parser::new();
        for b in b"a=t,f=32,s=1,v=1;AAAA" {
            p.feed(*b).unwrap();
        }
        let piece = p.finish().unwrap();
        assert_eq!(bulk.action, piece.action);
        assert_eq!(bulk.data, piece.data);
    }

    #[test]
    fn signed_z_parses_as_negative() {
        // 客户端按十进制负数发送层级（kitty 规范用法）。
        let cmd = parse("a=p,i=1,z=-1").unwrap();
        assert_eq!(cmd.control.i32(b'z'), Some(-1));
        assert!(cmd.control.i32(b'z').unwrap() < 0);
    }

    #[test]
    fn positive_z_is_also_signed() {
        let cmd = parse("a=p,i=1,z=3").unwrap();
        assert_eq!(cmd.control.i32(b'z'), Some(3));
    }
}
