//! 文件文本编码（编辑器、文件树缓存、传输层共用）。
//!
//! 放在 `utils` 是为了让 `file_manager` 不必反向依赖 `editor`。

use encoding_rs::{Encoding, GB18030, GBK, UTF_8, UTF_16BE, UTF_16LE};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

    pub(crate) fn encoder(&self) -> &'static Encoding {
        match self {
            Self::Utf8 | Self::Utf8Bom => UTF_8,
            Self::Utf16Le => UTF_16LE,
            Self::Utf16Be => UTF_16BE,
            Self::Gbk => GBK,
            Self::Gb18030 => GB18030,
        }
    }
}
