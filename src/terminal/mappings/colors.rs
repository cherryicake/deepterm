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
                .map(|rgb| rgb888_to_hsla(rgb))
                .unwrap_or_else(|| theme.named_color(*named)),
            AlacColor::Spec(rgb) => rgb888_to_hsla(*rgb),
            AlacColor::Indexed(idx) => self
                .get(*idx as usize)
                .map(|rgb| rgb888_to_hsla(rgb))
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

/// Convert alacritty color to GPUI Hsla
///
/// 兼容旧调用方的纯主题解析（不叠加调色板覆盖）。
/// 新渲染路径请使用 [`TerminalPalette::resolve`]。
pub fn convert_alacritty_color(color: &AlacColor, theme: &TerminalTheme) -> Hsla {
    match color {
        AlacColor::Named(named) => theme.named_color(*named),
        AlacColor::Spec(rgb) => rgb888_to_hsla(*rgb),
        AlacColor::Indexed(idx) => theme.indexed_color(*idx),
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
