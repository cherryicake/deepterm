//! 侧边栏共享视觉 token。
//!
//! 这里的颜色一律从当前主题派生，而不是硬编码的中性灰/白叠加：
//! - 材质底色取自 `panel_background`，并按明暗做轻微明度偏移（暗色下略亮、
//!   亮色下略暗），让侧边栏保持 macOS 式「浮在内容区之上」的层次；
//! - hover / 按钮等叠加态取自 `text` 的低透明度，明暗自动适配，并继承主题
//!   自身的冷暖倾向（Sepia 偏暖、Nord 偏冷）。
//!
//! 玻璃质感由四层构成（均为主题派生色）：
//! 1. 整高材质渐变 `glass_bg` —— 顶部微亮、底部微暗的连续明度坡度，避免
//!    纯色材质在暗色下呈「均匀深色块」、与上下边缘高光形成夹心式色阶跳变；
//! 2. hairline 描边 `border_color` —— 暗色下即玻璃边缘的亮边反光；
//! 3. 顶部反光渐变 `glass_top_highlight` —— 玻璃对环境光的顶部反射；
//! 4. 底部体积渐变 `glass_bottom_shade` —— 自下而上的微暗，营造厚度。
//!
//! 3/4 只做窄幅柔化：亮度主坡度已由 1 的整高渐变承担，边缘层负责锐化
//! 玻璃「受光面」的观感。二者由 `workspace::sidebar` 容器以 absolute 装饰层
//! 渲染；装饰层不注册任何鼠标监听，GPUI 中不会形成交互 hitbox，
//! 不会挡住下层交互。

use gpui::prelude::*;
use gpui::{
    Background, Div, Hsla, Stateful, div, linear_color_stop, linear_gradient, px, transparent_black,
};

use crate::assets::Icon;
use crate::theme::UiTheme;

/// 暗色主题材质不透明度：macOS 26 侧边栏通透感很强，取较低值让窗口级
/// 模糊背景（桌面色）充分透进来。适度提高以增强侧边栏与主背景的分离感。
const DARK_MATERIAL_ALPHA: f32 = 0.76;

/// 亮色主题材质不透明度：亮色下对比度更关键，需更不透以避免与白色主背景融合。
const LIGHT_MATERIAL_ALPHA: f32 = 0.92;

/// 暗色材质明度坡度：顶部相对 `panel_background` 提亮的幅度。
const DARK_TOP_LIFT: f32 = 0.078;

/// 暗色材质明度坡度：底部相对 `panel_background` 的幅度（仍略亮于面板，
/// 保持「浮起」层次，但比顶部暗，形成连续坡度而非色块）。
const DARK_BOTTOM_LIFT: f32 = 0.028;

/// 亮色材质明度坡度：顶部相对 `panel_background` 压暗的幅度。
/// 刻意拉大与底部的落差并整体加深，确保白色主题下侧边栏明显深于主背景。
const LIGHT_TOP_SHIFT: f32 = -0.042;

/// 亮色材质明度坡度：底部相对 `panel_background` 压暗的幅度。
const LIGHT_BOTTOM_SHIFT: f32 = -0.092;

/// 侧边栏材质底色：主题面板色 + 整高明度渐变 + 半透明。
///
/// 多数主题的 `panel_background` 与窗口底色是同一个值，若只做半透明，
/// 侧边栏会与背景融为一体。这里把明度坡度做进材质本身：顶部提亮
/// （暗色）/压暗（亮色）到玻璃「受光面」，向下连续过渡到略暗的底部，
/// 中段不再有均匀色块，上下边缘高光也能无缝衔接。
pub fn glass_bg(theme: UiTheme, is_dark: bool) -> Background {
    let (top, bottom, alpha) = if is_dark {
        (
            shift_lightness(theme.panel_background, DARK_TOP_LIFT),
            shift_lightness(theme.panel_background, DARK_BOTTOM_LIFT),
            DARK_MATERIAL_ALPHA,
        )
    } else {
        (
            shift_lightness(theme.panel_background, LIGHT_TOP_SHIFT),
            shift_lightness(theme.panel_background, LIGHT_BOTTOM_SHIFT),
            LIGHT_MATERIAL_ALPHA,
        )
    };
    linear_gradient(
        180.0,
        linear_color_stop(top.opacity(alpha), 0.0),
        linear_color_stop(bottom.opacity(alpha), 1.0),
    )
}

/// 侧边栏 hairline 描边。
///
/// 取 `text` 的低透明度：暗色主题下 text 接近白，形成玻璃边缘的亮边反光；
/// 亮色主题下 text 接近黑，即自然的深色 hairline。两种外观都贴合 macOS。
/// 亮色下略提高不透明度以强化与白色主背景的分界。
pub fn border_color(theme: UiTheme, is_dark: bool) -> Hsla {
    theme.text.opacity(if is_dark { 0.13 } else { 0.16 })
}

/// 顶部玻璃反光渐变：第一个色标（微亮）位于顶部，向下渐隐至全透明。
///
/// 亮色下使用纯白叠加（而非 text 深色）来提亮顶部受光面，与底部阴影形成
/// 明确的明暗体积感；暗色下沿用 text 白色，强度略高于旧值以锐化边缘。
pub fn glass_top_highlight(_theme: UiTheme, is_dark: bool) -> Background {
    let highlight: Hsla = if is_dark {
        // 暗色：白色受光，强度适中即可（材质本身已在顶部提亮）
        gpui::rgb(0xFFFFFF).into()
    } else {
        // 亮色：必须用白来对抗整体压暗，否则顶部的 text 深色会让顶部更脏
        gpui::rgb(0xFFFFFF).into()
    };
    let opacity = if is_dark { 0.055 } else { 0.32 };
    linear_gradient(
        180.0,
        linear_color_stop(highlight.opacity(opacity), 0.0),
        linear_color_stop(transparent_black(), 1.0),
    )
}

/// 底部体积渐变：第一个色标（微暗）位于底部，向上渐隐至全透明。
///
/// 使用中性黑色营造自下而上的厚度阴影，亮色下压暗底部以拉大与顶部的落差，
/// 暗色下则以更重的阴影压住底部，避免底部过亮导致悬浮感丢失。
pub fn glass_bottom_shade(_theme: UiTheme, is_dark: bool) -> Background {
    let shade: Hsla = gpui::rgb(0x000000).into();
    let opacity = if is_dark { 0.16 } else { 0.07 };
    linear_gradient(
        0.0,
        linear_color_stop(shade.opacity(opacity), 0.0),
        linear_color_stop(transparent_black(), 1.0),
    )
}

/// 列表行 / 菜单项 hover 叠加色。
pub fn hover_bg(theme: UiTheme) -> Hsla {
    theme.text.opacity(0.08)
}

/// 图标按钮底色。
pub fn button_bg(theme: UiTheme) -> Hsla {
    theme.text.opacity(0.10)
}

/// 图标按钮 hover 底色。
pub fn button_bg_hover(theme: UiTheme) -> Hsla {
    theme.text.opacity(0.15)
}

/// 侧边栏顶部 28px 圆形图标按钮。
///
/// 只复用视觉样式；调用方继续绑定 `id`、点击/鼠标事件，避免把不同的
/// 交互语义塞进同一个泛型回调参数。
pub(crate) fn round_icon_button(theme: UiTheme, icon: Icon, icon_size: f32) -> Div {
    div()
        .w(px(28.0))
        .h(px(28.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(999.0))
        .bg(button_bg(theme))
        .cursor_pointer()
        .hover(move |style| style.bg(button_bg_hover(theme)))
        .child(
            gpui::svg()
                .size(px(icon_size))
                .path(icon.path())
                .text_color(theme.text),
        )
}

/// 列表行的选中 / 悬停视觉状态。
///
/// 选中行使用 accent 实底；未选中行只保留 hover 叠加与常规文字色。
/// 行尺寸、内边距和内容布局仍由各调用点负责。
pub(crate) fn row_selection_state(
    theme: UiTheme,
    is_selected: bool,
) -> impl FnOnce(Stateful<Div>) -> Stateful<Div> {
    move |row| {
        if is_selected {
            row.bg(theme.accent).text_color(theme.accent_foreground)
        } else {
            row.hover(move |style| style.bg(hover_bg(theme)))
                .text_color(theme.text)
        }
    }
}

/// 在 HSL 明度上做 ±delta 偏移，色相与饱和度保持不变。
fn shift_lightness(color: Hsla, delta: f32) -> Hsla {
    Hsla {
        l: (color.l + delta).clamp(0.0, 1.0),
        ..color
    }
}
