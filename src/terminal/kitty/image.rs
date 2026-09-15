//! 图形负载的解码：RGB / RGBA / PNG → 统一的 RGBA 像素缓冲。

use super::command::{Format, ParseError};

/// 图像 id 类型（`i=` 键的存储键）。取值 0 表示未显式指定。
pub type ImageId = u32;

/// 单张图像的字节上限（对齐 ghostty `image.zig` 的 400 MB）。
pub const MAX_IMAGE_BYTES: usize = 400 * 1024 * 1024;

/// 单张图像的像素数上限，防解压炸弹。
/// 与 `editor/image_viewer.rs` 的 `MAX_IMAGE_PIXELS` 取同一量级（8000×8000）。
pub const MAX_IMAGE_PIXELS: u64 = 8000 * 8000;

/// 解码后的图像：始终是 RGBA8 行主序。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// 长度恒为 `width * height * 4`。
    pub rgba: Vec<u8>,
}

impl DecodedImage {
    /// 像素字节数。
    pub fn byte_len(&self) -> usize {
        self.rgba.len()
    }
}

/// 解码错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// 声明的格式无对应 payload 规则。
    UnsupportedFormat,
    /// 负载长度与宽高/格式不匹配。
    LengthMismatch,
    /// 尺寸缺失（非 PNG 必须给 `s=`/`v=`）。
    MissingDimensions,
    /// 尺寸或像素数超限。
    TooLarge,
    /// PNG 解码失败。
    InvalidPng,
    /// `o=z` zlib 流解压失败（对齐 ghostty `error.DecompressionFailed`）。
    DecompressFailed,
}

impl DecodeError {
    /// kitty 响应里的错误文本。
    pub fn message(self) -> &'static str {
        match self {
            Self::UnsupportedFormat => "EINVAL: unsupported format",
            Self::LengthMismatch => "EINVAL: data length mismatch",
            Self::MissingDimensions => "EINVAL: missing dimensions",
            Self::TooLarge => "EINVAL: too large",
            Self::InvalidPng => "EINVAL: invalid PNG",
            Self::DecompressFailed => "EINVAL: decompression failed",
        }
    }
}

impl From<DecodeError> for ParseError {
    fn from(e: DecodeError) -> Self {
        match e {
            DecodeError::TooLarge => ParseError::TooLarge,
            _ => ParseError::InvalidFormat,
        }
    }
}

/// 解压 `o=z` 的 zlib 负载（对齐 ghostty `graphics_image.zig::decompressZlib`）。
///
/// 输出上限取 [`MAX_IMAGE_BYTES`]——像素尺寸校验在之后的 `decode` 里还会做一次，
/// 但解压发生在尺寸校验之前，必须在此先挡住"小声明 + 巨大解压流"的解压炸弹。
/// 逐块读取、超限即停，不会为恶意流分配无界内存。
pub fn decompress_zlib(data: &[u8]) -> Result<Vec<u8>, DecodeError> {
    use std::io::Read as _;

    let mut out = Vec::new();
    let mut decoder = flate2::read::ZlibDecoder::new(data);
    // 16 KiB 一个块：正常路径每次 read 追加，超限路径最多多解压一块即放弃。
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match decoder.read(&mut chunk) {
            Ok(0) => return Ok(out),
            Ok(n) => {
                if out.len() + n > MAX_IMAGE_BYTES {
                    return Err(DecodeError::TooLarge);
                }
                out.extend_from_slice(&chunk[..n]);
            }
            Err(_) => return Err(DecodeError::DecompressFailed),
        }
    }
}

/// 按声明格式解码负载。
///
/// `width`/`height` 来自 `s=`/`v=`，PNG 可省略（从文件头读取）。
pub fn decode(
    data: &[u8],
    format: Format,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<DecodedImage, DecodeError> {
    match format {
        Format::Png => decode_png(data),
        Format::Rgb | Format::Rgba => {
            let (w, h) = match (width, height) {
                (Some(w), Some(h)) => (w, h),
                _ => return Err(DecodeError::MissingDimensions),
            };
            check_dimensions(w, h)?;
            decode_raw(data, format, w, h)
        }
    }
}

/// 校验尺寸与像素数上限。
fn check_dimensions(width: u32, height: u32) -> Result<(), DecodeError> {
    if width == 0 || height == 0 {
        return Err(DecodeError::MissingDimensions);
    }
    let pixels = width as u64 * height as u64;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(DecodeError::TooLarge);
    }
    // 上限之内，`pixels * 4` 不会溢出 usize（64 位）。
    if pixels * 4 > MAX_IMAGE_BYTES as u64 {
        return Err(DecodeError::TooLarge);
    }
    Ok(())
}

/// 解码未压缩的 RGB / RGBA。
fn decode_raw(
    data: &[u8],
    format: Format,
    width: u32,
    height: u32,
) -> Result<DecodedImage, DecodeError> {
    let pixels = width as usize * height as usize;
    let bpp = match format {
        Format::Rgb => 3,
        Format::Rgba => 4,
        Format::Png => unreachable!("PNG 走独立解码路径"),
    };

    let expected = pixels * bpp;
    // 允许负载**多于**声明（部分客户端按行对齐填充），但绝不能少。
    if data.len() < expected {
        return Err(DecodeError::LengthMismatch);
    }

    let rgba = if bpp == 4 {
        data[..expected].to_vec()
    } else {
        let mut out = Vec::with_capacity(pixels * 4);
        for px in data[..expected].chunks_exact(3) {
            out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
        }
        out
    };

    Ok(DecodedImage {
        width,
        height,
        rgba,
    })
}

/// 解码 PNG（`f=100`）。
fn decode_png(data: &[u8]) -> Result<DecodedImage, DecodeError> {
    use image::ImageReader;

    let reader = ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(|_| DecodeError::InvalidPng)?;
    let dynamic = reader.decode().map_err(|_| DecodeError::InvalidPng)?;

    let (width, height) = (dynamic.width(), dynamic.height());
    check_dimensions(width, height)?;

    let rgba = dynamic.to_rgba8().into_raw();
    Ok(DecodedImage {
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_rgba_raw() {
        let data = [255u8, 0, 0, 255];
        let img = decode(&data, Format::Rgba, Some(1), Some(1)).unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.rgba, vec![255, 0, 0, 255]);
    }

    #[test]
    fn decodes_rgb_raw_to_rgba() {
        let data = [10u8, 20, 30];
        let img = decode(&data, Format::Rgb, Some(1), Some(1)).unwrap();
        assert_eq!(img.rgba, vec![10, 20, 30, 255], "RGB 应补不透明 alpha");
    }

    #[test]
    fn short_payload_is_rejected() {
        // 声明 2 像素但只给 1 像素的数据。
        let data = [1u8, 2, 3, 4];
        assert_eq!(
            decode(&data, Format::Rgba, Some(2), Some(1)),
            Err(DecodeError::LengthMismatch)
        );
    }

    #[test]
    fn oversized_payload_is_accepted_when_padded() {
        // 负载多于声明（行对齐填充）应被容忍。
        let data = [1u8, 2, 3, 4, 9, 9, 9, 9];
        let img = decode(&data, Format::Rgba, Some(1), Some(1)).unwrap();
        assert_eq!(img.rgba, vec![1, 2, 3, 4]);
    }

    #[test]
    fn missing_dimensions_is_rejected() {
        assert_eq!(
            decode(&[1u8, 2, 3, 4], Format::Rgba, None, Some(1)),
            Err(DecodeError::MissingDimensions)
        );
    }

    #[test]
    fn zero_dimensions_rejected() {
        assert_eq!(
            decode(&[], Format::Rgba, Some(0), Some(0)),
            Err(DecodeError::MissingDimensions)
        );
    }

    #[test]
    fn oversized_dimensions_rejected() {
        assert_eq!(
            decode(&[], Format::Rgba, Some(100_000), Some(100_000)),
            Err(DecodeError::TooLarge)
        );
    }

    #[test]
    fn decodes_png() {
        // 1×1 红色 PNG。
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let img = decode(png, Format::Png, None, None).unwrap();
        assert_eq!(img.width, 1);
        assert_eq!(img.height, 1);
        assert_eq!(img.rgba.len(), 4);
    }

    #[test]
    fn invalid_png_is_rejected() {
        assert_eq!(
            decode(b"not a png", Format::Png, None, None),
            Err(DecodeError::InvalidPng)
        );
    }

    #[test]
    fn zlib_roundtrip_decompresses() {
        use std::io::Write as _;

        let raw = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&raw).unwrap();
        let compressed = enc.finish().unwrap();
        assert_eq!(decompress_zlib(&compressed).unwrap(), raw);
    }

    #[test]
    fn zlib_garbage_is_rejected() {
        assert_eq!(
            decompress_zlib(b"not a zlib stream"),
            Err(DecodeError::DecompressFailed)
        );
    }

    #[test]
    fn zlib_empty_stream_decompresses_to_empty() {
        // 空输入的 zlib 流是合法的（解压为空）；后续 decode 会因长度不足拒绝。
        use std::io::Write as _;

        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&[]).unwrap();
        let compressed = enc.finish().unwrap();
        assert_eq!(decompress_zlib(&compressed).unwrap(), Vec::<u8>::new());
    }
}
