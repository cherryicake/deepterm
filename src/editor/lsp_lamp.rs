//! LSP 状态指示灯：中心一个实心点 + 一圈从中心向外扩散的涟漪。
//!
//! 状态栏的 chip 与服务器控制菜单共用这一个组件——「灯」只有一种长相，
//! 颜色才是状态。分开实现的话，两边迟早会长得不一样（菜单里的点静止、
//! 状态栏里的点会动），用户没法把两者联系起来。
//!
//! 三层考虑：
//! - **涟漪用绝对定位**：它比槽位大，靠 flex 居中被溢出规则坑过（中心化
//!   的 flex 子项超框时可能只往一侧溢出）。这里自己算 `top/left` 偏移，
//!   保证向四周对称扩散，也不会把旁边的文字挤开。
//! - **涟漪画在实心点下面**：起点时两者一样大，实心点正好把它盖住，看不出
//!   一圈突兀的圆；同时也让 `reduce_motion`（`delta = 0`）降级成一个朴素的
//!   点，而不是半个残环。
//! - **节奏不随状态变**：出错只换颜色，扩散速度不变。状态一变动画也变的
//!   话，用户会以为"变慢了"是另一层信息。

use gpui::prelude::*;
use gpui::{Animation, AnimationExt as _, AnyElement, Hsla, div, px};
use std::time::Duration;

/// 指示灯槽位的边长（px）。
///
/// 涟漪在这个范围内扩散，所以**必须不小于** [`RIPPLE_MAX`]。固定尺寸
/// 是为了让 chip 高度和旁边的缩进/编码 chip 一致——指示灯不该把状态栏撑高。
const SLOT: f32 = 14.0;

/// 指示灯中心实心点的边长（px）：灯的"芯"。
const CORE: f32 = 5.0;

/// 涟漪环的最大直径（px）。
const RIPPLE_MAX: f32 = 14.0;

/// 涟漪的周期（毫秒）：一圈扩散 + 淡出的时间。
///
/// 1800ms 是刻意的慢——LSP 是常驻状态，不是等待中的进度条。快于 1s 会跟
/// 旁边 tab 的 loading 转圈抢注意力，反而让人以为它在忙。
const RIPPLE_PERIOD_MS: u64 = 1800;

/// 涟漪环起始（贴着核心点）时的不透明度。
const RIPPLE_ALPHA: f32 = 0.75;

/// 渲染一盏指示灯。
///
/// `color` 是状态色（运行中绿 / 启动中蓝 / 失败红 / 已停止灰）。
pub fn lamp(color: Hsla) -> AnyElement {
    div()
        .relative()
        .w(px(SLOT))
        .h(px(SLOT))
        .flex()
        .items_center()
        .justify_center()
        .child(ripple(color))
        .child(div().w(px(CORE)).h(px(CORE)).rounded_full().bg(color))
        .into_any_element()
}

/// 外扩的涟漪环。
///
/// 与 [`lamp`] 分开只为一件事：动画的作用对象是这一圈，而不是外层槽位——
/// 混在一个函数里的话，"动画在动谁" 会被槽位的固定尺寸盖住。
fn ripple(color: Hsla) -> impl gpui::IntoElement {
    div()
        .absolute()
        .w(px(CORE))
        .h(px(CORE))
        .rounded_full()
        .border(px(1.0))
        .border_color(color)
        .with_animation(
            "lsp-status-ripple",
            Animation::new(Duration::from_millis(RIPPLE_PERIOD_MS))
                .repeat()
                // 慢速形变，30fps 足够顺滑；不加这个限制的话它会跟着
                // 120Hz 屏每帧重绘整个 chip。
                .with_max_fps(30.0),
            move |this, delta| {
                let size = size_at(delta);
                // 居中所算偏移：槽位固定，涟漪直径在变。
                let offset = (SLOT - size) * 0.5;
                this.w(px(size))
                    .h(px(size))
                    .top(px(offset))
                    .left(px(offset))
                    .opacity(alpha_at(delta))
            },
        )
}

/// 涟漪在 `t`（0..1）时刻的直径。
///
/// 线性扩散：涟漪是"波"，等速外扩才像声纳；做缓动会像气球在吹大。
fn size_at(t: f32) -> f32 {
    CORE + (RIPPLE_MAX - CORE) * t
}

/// 涟漪在 `t` 时刻的不透明度：一边扩散一边淡出。
///
/// `t = 1` 时必须是 0。`Animation::repeat()` 把归一化时间取模回 0，直径会在
/// 接缝处从 [`RIPPLE_MAX`] 跳回 [`CORE`]——只有淡到看不见时再跳，这一下才
/// 不会被看见。
fn alpha_at(t: f32) -> f32 {
    RIPPLE_ALPHA * (1.0 - t)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 槽位必须装得下整个涟漪，否则扩散到一半会被裁掉。
    ///
    /// 居中所算的偏移 `(SLOT - size) / 2` 依赖这一点：槽位小于涟漪时偏移为
    /// 负，圆会跳出槽位、把旁边的文字盖住。
    ///
    /// `RIPPLE_MAX <= SLOT` 是编译期常量比较，本质是「改大涟漪时别忘了改
    /// 槽位」的护栏；真正的运行期断言是逐时刻检查偏移不为负。
    #[test]
    fn ripple_never_overflows_its_slot() {
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let offset = (SLOT - size_at(t)) * 0.5;
            assert!(offset >= 0.0, "t={t} 偏移为负: {offset}");
        }
        const { assert!(RIPPLE_MAX <= SLOT) };
    }

    /// 涟漪从核心点开始、一路扩到槽位大小。
    #[test]
    fn ripple_spans_core_to_max() {
        assert!((size_at(0.0) - CORE).abs() < 1e-6);
        assert!((size_at(1.0) - RIPPLE_MAX).abs() < 1e-6);
    }

    /// 周期结束时涟漪必须已经完全透明。
    ///
    /// 否则 `Animation::repeat()` 取模回 0 时，直径从最大跳回核心的那一下
    /// 会闪。
    #[test]
    fn ripple_fades_out_before_the_cycle_wraps() {
        assert!(alpha_at(1.0).abs() < 1e-6);
        assert!(alpha_at(0.0) > 0.0);
        for i in 1..=10 {
            let (prev, now) = (alpha_at((i - 1) as f32 / 10.0), alpha_at(i as f32 / 10.0));
            assert!(now < prev, "不透明度应单调下降: {prev} → {now}");
        }
    }

    /// `reduce_motion` 降级要干净。
    ///
    /// GPUI 对循环动画在 reduce_motion 时直接取周期起点（`delta = 0`，见
    /// `gpui::AnimationElement`）。此时涟漪直径与灯芯**完全一致**，而灯芯画
    /// 在它上面——正好被盖住，用户看到的就是一个朴素的点。
    #[test]
    fn ripple_is_hidden_behind_the_core_when_motion_is_reduced() {
        assert!(
            (size_at(0.0) - CORE).abs() < 1e-6,
            "reduce_motion 下涟漪直径必须等于灯芯直径，否则会露出一圈残环"
        );
    }
}
