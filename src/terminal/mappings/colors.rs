//! Color mappings for terminal

use alacritty_terminal::term::color::COUNT as COLOR_COUNT;
use alacritty_terminal::vte::ansi::{Color as AlacColor, NamedColor, Rgb};
use gpui::Hsla;

use crate::theme::TerminalTheme;

/// 终端当前生效调色板快照（`Term.colors` 的 [Option<Rgb>; 269] 拷贝）。
///
/// ## 语义
/// - 初始化 / 主题切换时，由 [`Terminal::apply_theme_colors`] 把主题色
///   （16 基础色 + 前景 / 背景 / 光标 / 暗色 / 亮前景）灌入 `Term.colors`；
/// - 应用随后可通过 OSC 4 / 10 / 11 / 12 修改其中任意槽位（vte 写入
///   `Term.colors`，并自动标记全量 damage）；
/// - 应用可通过 OSC 104 / 110 / 111 重置——重置把槽位清为 `None`，
///   此时解析回退到**当前主题**（正是重置的正确语义：回到终端默认色）。
///
/// 因此渲染路径解析任何 `Color::Named` / `Color::Indexed(0..=255)` 时先查
/// 本快照（应用与主题叠加后的当前值），`None` 再回退主题静态色。
///
/// `Copy` 类型：`update_last_content` 持 term 锁期间一次拷贝进
/// `TerminalContent.palette`，渲染热路径零锁读取。
#[derive(Clone, Copy, Debug)]
pub struct TerminalPalette {
    entries: [Option<Rgb>; COLOR_COUNT],
}

impl Default for TerminalPalette {
    fn default() -> Self {
        Self {
            entries: [None; COLOR_COUNT],
        }
    }
}

impl TerminalPalette {
    /// 直接从 alacritty `Colors` 全量拷贝构造。
    pub fn from_colors(colors: &alacritty_terminal::term::color::Colors) -> Self {
        let mut entries = [None; COLOR_COUNT];
        for (index, dst) in entries.iter_mut().enumerate() {
            *dst = colors[index];
        }
        Self { entries }
    }

    /// 调色板内容指纹，用于行缓存失效。
    ///
    /// 行缓存复用只看 cell 内容指纹；OSC 4/10/11/12 改色不改变 cell 的
    /// `Color` 枚举，却会改变最终解析出的颜色。没有这个指纹，改色后的
    /// 全量 damage 会复用旧颜色，TUI 调色板看起来“串色/叠色”。
    pub fn hash_u64(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for entry in &self.entries {
            match entry {
                Some(rgb) => {
                    hash ^= 1;
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                    hash ^= u64::from(
                        u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b),
                    );
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                }
                None => {
                    hash ^= 0;
                    hash = hash.wrapping_mul(0x100_0000_01b3);
                }
            }
        }
        hash
    }

    /// 查询指定索引（0..=255 为调色板 / 256..=268 为命名扩展色），
    /// 未被设置（None）时返回 None，由调用方回退主题。
    #[inline]
    pub fn get(&self, index: usize) -> Option<Rgb> {
        self.entries.get(index).copied().flatten()
    }

    /// 解析 alacritty 颜色为最终显示色：调色板覆盖（应用 OSC 改色 /
    /// 主题灌入）优先，未覆盖回退主题静态色。
    #[inline]
    pub fn resolve(&self, color: &AlacColor, theme: &TerminalTheme) -> Hsla {
        match color {
            AlacColor::Named(named) => self
                .get(*named as usize)
                .map(rgb888_to_hsla)
                .unwrap_or_else(|| theme.named_color(*named)),
            AlacColor::Spec(rgb) => rgb888_to_hsla(*rgb),
            AlacColor::Indexed(idx) => self
                .get(*idx as usize)
                .map(rgb888_to_hsla)
                .unwrap_or_else(|| theme.indexed_color(*idx)),
        }
    }
}

/// u8 RGB → GPUI Hsla（不透明）。
#[inline]
pub fn rgb888_to_hsla(rgb: Rgb) -> Hsla {
    gpui::rgb((rgb.r as u32) << 16 | (rgb.g as u32) << 8 | rgb.b as u32).into()
}

/// GPUI Hsla → u8 RGB（丢弃 alpha，终端色板不含透明度）。
#[inline]
pub fn hsla_to_rgb888(color: Hsla) -> Rgb {
    let rgba = color.to_rgb();
    Rgb {
        r: (rgba.r.clamp(0., 1.) * 255.).round() as u8,
        g: (rgba.g.clamp(0., 1.) * 255.).round() as u8,
        b: (rgba.b.clamp(0., 1.) * 255.).round() as u8,
    }
}

/// 把主题色板灌入 alacritty `Term.colors` 的槽位清单。
///
/// 返回 `(索引, rgb)` 列表，索引含义见 `alacritty_terminal::term::color::Colors`：
/// 0..=15 基础色 / 256 前景 / 257 背景 / 258 光标 / 259..=266 暗色 / 267 亮前景。
/// 其余槽位（16..=255 色立方与灰阶）留空——查询时由
/// [`TerminalPalette::resolve`] 回退到主题的 256 色静态板。
pub fn theme_color_overrides(theme: &TerminalTheme) -> Vec<(usize, Rgb)> {
    let mut out = Vec::with_capacity(27);
    let normal: [Hsla; 8] = [
        theme.named_color(NamedColor::Black),
        theme.named_color(NamedColor::Red),
        theme.named_color(NamedColor::Green),
        theme.named_color(NamedColor::Yellow),
        theme.named_color(NamedColor::Blue),
        theme.named_color(NamedColor::Magenta),
        theme.named_color(NamedColor::Cyan),
        theme.named_color(NamedColor::White),
    ];
    let bright: [Hsla; 8] = [
        theme.named_color(NamedColor::BrightBlack),
        theme.named_color(NamedColor::BrightRed),
        theme.named_color(NamedColor::BrightGreen),
        theme.named_color(NamedColor::BrightYellow),
        theme.named_color(NamedColor::BrightBlue),
        theme.named_color(NamedColor::BrightMagenta),
        theme.named_color(NamedColor::BrightCyan),
        theme.named_color(NamedColor::BrightWhite),
    ];
    let dim: [Hsla; 8] = [
        theme.named_color(NamedColor::DimBlack),
        theme.named_color(NamedColor::DimRed),
        theme.named_color(NamedColor::DimGreen),
        theme.named_color(NamedColor::DimYellow),
        theme.named_color(NamedColor::DimBlue),
        theme.named_color(NamedColor::DimMagenta),
        theme.named_color(NamedColor::DimCyan),
        theme.named_color(NamedColor::DimWhite),
    ];

    for (i, c) in normal.iter().chain(bright.iter()).enumerate() {
        out.push((i, hsla_to_rgb888(*c)));
    }
    out.push((
        NamedColor::Foreground as usize,
        hsla_to_rgb888(theme.foreground),
    ));
    out.push((
        NamedColor::Background as usize,
        hsla_to_rgb888(theme.background),
    ));
    out.push((NamedColor::Cursor as usize, hsla_to_rgb888(theme.cursor)));
    for (i, c) in dim.iter().enumerate() {
        out.push((NamedColor::DimBlack as usize + i, hsla_to_rgb888(*c)));
    }
    out.push((
        NamedColor::BrightForeground as usize,
        hsla_to_rgb888(theme.bright_foreground),
    ));
    out.push((
        NamedColor::DimForeground as usize,
        hsla_to_rgb888(theme.dim_foreground),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_hash_changes_when_osc_override_changes() {
        let base = TerminalPalette::default().hash_u64();
        let mut colors = alacritty_terminal::term::color::Colors::default();
        colors[1] = Some(Rgb { r: 1, g: 2, b: 3 });
        let overridden = TerminalPalette::from_colors(&colors).hash_u64();
        assert_ne!(base, overridden, "OSC 改色必须改变调色板指纹");
        colors[1] = Some(Rgb { r: 1, g: 2, b: 4 });
        assert_ne!(
            overridden,
            TerminalPalette::from_colors(&colors).hash_u64(),
            "颜色值变化也必须改变指纹"
        );
    }
}
