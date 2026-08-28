//! 文件编码检测与转换。
//!
//! 读取：BOM → UTF-8 → GBK → GB18030 逐级检测，解码为编辑器内的 UTF-8 文本。
//! 保存：按检测到的编码重新编码写盘（UTF-8 BOM / UTF-16 保留 BOM），
//! 含无法表示的字符时报告错误，由调用方决定阻止保存（避免静默替换 `?`
//! 破坏文件内容）。

use encoding_rs::{Encoding, UTF_16BE, UTF_16LE};
use serde::{Deserialize, Serialize};

/// 文件文本编码。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub enum FileEncoding {
    #[default]
    Utf8,
    /// 带 BOM 的 UTF-8（保存时写回 BOM）。
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Gbk,
    /// GB18030（GBK 超集，覆盖四字节区）。
    Gb18030,
}

impl FileEncoding {
    /// 状态栏 / UI 显示名。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8 BOM",
            Self::Utf16Le => "UTF-16 LE",
            Self::Utf16Be => "UTF-16 BE",
            Self::Gbk => "GBK",
            Self::Gb18030 => "GB18030",
        }
    }

    fn encoder(&self) -> &'static Encoding {
        match self {
            Self::Utf8 | Self::Utf8Bom => encoding_rs::UTF_8,
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            Self::Gbk => encoding_rs::GBK,
            Self::Gb18030 => encoding_rs::GB18030,
        }
    }
}

/// 解码结果：编辑器文本 + 源编码。
pub struct DecodedText {
    pub text: String,
    pub encoding: FileEncoding,
}

/// 检测字节序列的文本编码并解码。
///
/// 检测顺序：
/// 1. BOM（UTF-8 BOM / UTF-16 LE / UTF-16 BE）
/// 2. 严格 UTF-8 校验
/// 3. GBK 严格解码
/// 4. GB18030 严格解码
/// 5. 兜底按 UTF-8 有损解码（与旧行为一致，`had_errors` 场景由二进制
///    检测先行拦截）
///
/// 调用方应先用 [`is_binary_bytes`] 拦截二进制内容。
pub fn decode_bytes(bytes: &[u8]) -> DecodedText {
    if bytes.is_empty() {
        return DecodedText {
            text: String::new(),
            encoding: FileEncoding::Utf8,
        };
    }

    // ---- BOM 检测（优先级最高）
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        let (text, had_errors) = encoding_rs::UTF_8.decode_with_bom_removal(bytes);
        let _ = had_errors;
        return DecodedText {
            text: text.into_owned(),
            encoding: FileEncoding::Utf8Bom,
        };
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let (text, had_errors) = UTF_16LE.decode_with_bom_removal(bytes);
        let _ = had_errors;
        return DecodedText {
            text: text.into_owned(),
            encoding: FileEncoding::Utf16Le,
        };
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let (text, had_errors) = UTF_16BE.decode_with_bom_removal(bytes);
        let _ = had_errors;
        return DecodedText {
            text: text.into_owned(),
            encoding: FileEncoding::Utf16Be,
        };
    }

    // ---- 严格 UTF-8
    if let Ok(text) = std::str::from_utf8(bytes) {
        return DecodedText {
            text: text.to_string(),
            encoding: FileEncoding::Utf8,
        };
    }

    // ---- GBK（先于 GB18030：GBK 是更常见、更严格的子集）
    let (text, had_errors) = encoding_rs::GBK.decode_without_bom_handling(bytes);
    if !had_errors {
        // 往返校验：encoding_rs 的 GBK 解码器接受 GB18030 四字节序列，
        // 但 GBK 编码器无法将其写回；这种文件应识别为 GB18030，
        // 否则保存时会被错误拦截。
        let (_, encode_errors) = encode_text(&text, FileEncoding::Gbk);
        if !encode_errors {
            return DecodedText {
                text: text.into_owned(),
                encoding: FileEncoding::Gbk,
            };
        }
    }

    // ---- GB18030（四字节区字符）
    let (text, had_errors) = encoding_rs::GB18030.decode_without_bom_handling(bytes);
    if !had_errors {
        return DecodedText {
            text: text.into_owned(),
            encoding: FileEncoding::Gb18030,
        };
    }

    // ---- 兜底：UTF-8 有损（与旧 from_utf8_lossy 行为一致）
    DecodedText {
        text: String::from_utf8_lossy(bytes).into_owned(),
        encoding: FileEncoding::Utf8,
    }
}

/// 按指定编码编码文本。
///
/// 返回 `(字节, had_errors)`：`had_errors = true` 表示存在该编码无法表示的
/// 字符（已被替换为 `?`），调用方应阻止保存以防静默破坏内容。
pub fn encode_text(text: &str, encoding: FileEncoding) -> (Vec<u8>, bool) {
    let mut bytes = Vec::new();
    match encoding {
        FileEncoding::Utf8Bom => bytes.extend_from_slice(&[0xEF, 0xBB, 0xBF]),
        FileEncoding::Utf16Le | FileEncoding::Utf16Be => {
            // 注意：不能用 Encoding::encode()：按 WHATWG 规范它的「输出编码」
            // 对 UTF-16 一律是 UTF-8（表单提交语义）。手动编码（含代理对）。
            // BOM 前置。
            bytes.extend_from_slice(match encoding {
                FileEncoding::Utf16Le => &[0xFF, 0xFE],
                _ => &[0xFE, 0xFF],
            });
            for unit in text.encode_utf16() {
                match encoding {
                    FileEncoding::Utf16Le => bytes.extend_from_slice(&unit.to_le_bytes()),
                    _ => bytes.extend_from_slice(&unit.to_be_bytes()),
                }
            }
            // UTF-16 可表示所有 Unicode 标量值（代理对），不会失败。
            return (bytes, false);
        }
        _ => {}
    }
    let (encoded, _, had_errors) = encoding.encoder().encode(text);
    bytes.extend_from_slice(&encoded);
    (bytes, had_errors)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_utf8() {
        let d = decode_bytes("hello 世界\n".as_bytes());
        assert_eq!(d.encoding, FileEncoding::Utf8);
        assert_eq!(d.text, "hello 世界\n");
    }

    #[test]
    fn detect_utf8_bom() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("带 BOM 的文本".as_bytes());
        let d = decode_bytes(&bytes);
        assert_eq!(d.encoding, FileEncoding::Utf8Bom);
        assert_eq!(d.text, "带 BOM 的文本");
        // BOM 不混入正文
        assert!(!d.text.starts_with('\u{feff}'));
    }

    #[test]
    fn utf8_bom_roundtrip() {
        let (bytes, err) = encode_text("内容", FileEncoding::Utf8Bom);
        assert!(!err);
        assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF]);
        let d = decode_bytes(&bytes);
        assert_eq!(d.encoding, FileEncoding::Utf8Bom);
        assert_eq!(d.text, "内容");
    }

    #[test]
    fn detect_utf16_le_bom() {
        let (bytes, _) = encode_text("UTF16 文本", FileEncoding::Utf16Le);
        assert_eq!(&bytes[..2], &[0xFF, 0xFE]);
        let d = decode_bytes(&bytes);
        assert_eq!(d.encoding, FileEncoding::Utf16Le);
        assert_eq!(d.text, "UTF16 文本");
    }

    #[test]
    fn detect_utf16_be_bom() {
        let (bytes, _) = encode_text("大端", FileEncoding::Utf16Be);
        assert_eq!(&bytes[..2], &[0xFE, 0xFF]);
        let d = decode_bytes(&bytes);
        assert_eq!(d.encoding, FileEncoding::Utf16Be);
        assert_eq!(d.text, "大端");
    }

    #[test]
    fn detect_gbk() {
        // "你好，世界" 的 GBK 编码
        let gbk_bytes: Vec<u8> = "\u{4f60}\u{597d}\u{ff0c}\u{4e16}\u{754c}"
            .chars()
            .flat_map(|c| {
                let (b, _) = encode_text(&c.to_string(), FileEncoding::Gbk);
                b
            })
            .collect();
        let d = decode_bytes(&gbk_bytes);
        assert_eq!(d.encoding, FileEncoding::Gbk);
        assert_eq!(d.text, "你好，世界");

        let (round, err) = encode_text(&d.text, FileEncoding::Gbk);
        assert!(!err);
        assert_eq!(round, gbk_bytes);
    }

    #[test]
    fn gbk_unencodable_reports_error() {
        // emoji 不在 GBK 字符集内
        let (_, err) = encode_text("😀", FileEncoding::Gbk);
        assert!(err);
    }

    #[test]
    fn detect_gb18030_four_byte() {
        // U+20024（𠀤 类扩展区字符）在 GBK 双字节区之外，GB18030 用四字节表示
        let s = "\u{20024}";
        let (bytes, err) = encode_text(s, FileEncoding::Gb18030);
        assert!(!err);
        // GBK 无法解码（会落到 GB18030 分支）
        let d = decode_bytes(&bytes);
        assert_eq!(d.encoding, FileEncoding::Gb18030);
        assert_eq!(d.text, s);
    }

    #[test]
    fn invalid_bytes_fallback_utf8_lossy() {
        // 孤立的 UTF-8 连续字节 + 不完整 GBK 序列 → 有损 UTF-8 兜底
        let d = decode_bytes(&[0x61, 0xFF, 0x62]);
        // 0xFF 单独出现：GBK 判 invalid（0xFF 不是合法首字节前缀……实际 0xFF 不是 GBK lead byte）
        // 无论落入哪个分支，必须能打开且不 panic。
        assert!(!d.text.is_empty());
    }

    #[test]
    fn empty_file_is_utf8() {
        let d = decode_bytes(&[]);
        assert_eq!(d.encoding, FileEncoding::Utf8);
        assert!(d.text.is_empty());
    }
}
