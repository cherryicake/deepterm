//! hover 浮层「指针离开目标」分支的测试。
//!
//! 对照 Zed `hover_popover.rs` 的 `test_hover_popover_hiding_delay*` 系列。
//! 判定逻辑之所以抽成 [`HoverState::off_target_action`]，就是为了能脱离
//! `Editor`（它要文件、标签页、LSP 会话）直接驱动这三支。

use gpui::{Bounds, Pixels, Point, px, size};

use super::hover::{HoverState, OffTargetAction};

/// 一张 200×50 的浮层：中心 (200, 125)，半宽 100、半高 25。
fn popover_bounds() -> Bounds<Pixels> {
    Bounds::new(Point::new(px(100.), px(100.)), size(px(200.), px(50.)))
}

/// 浮层正下方 100px 处（矩形外，距离 = |250-125| - 25 = 100）。
fn near_point() -> Point<Pixels> {
    Point::new(px(200.), px(250.))
}

/// 冲过头之后更远的地方（距离 350）。
fn far_point() -> Point<Pixels> {
    Point::new(px(200.), px(500.))
}

/// 折返了一点、但仍比历史最近距离远 10px（距离 110 > 最近 100 + 4）。
fn back_off_point() -> Point<Pixels> {
    Point::new(px(200.), px(260.))
}

/// 真的在靠近（距离 90 < 最近 100 + 4）。
fn closer_point() -> Point<Pixels> {
    Point::new(px(200.), px(240.))
}

/// 没有卡片在显示 → 作废在飞请求（Zed `info_task = None`）。
///
/// 少了这一支，指针已经移到行号栏上了，在途请求还会回来把卡片弹出来。
#[test]
fn off_target_without_popover_cancels_inflight_request() {
    let mut hover = HoverState::new();
    assert_eq!(
        hover.off_target_action(false, Some(near_point()), Some(popover_bounds())),
        OffTargetAction::CancelRequest,
        "没有卡片时指针离开正文必须作废在飞请求"
    );
}

/// 指针在走远、且计时器已排 → 让它跑完，不许往后推。
///
/// 少了这一支（Zed `if !getting_closer && hiding_delay_task.is_some() { return }`），
/// 指针每动一下都重启计时器，卡片永远收不掉。
#[gpui::test]
fn off_target_moving_away_lets_the_hide_timer_run(cx: &mut gpui::TestAppContext) {
    cx.update(|cx: &mut gpui::App| {
        let mut hover = HoverState::new();
        let bounds = Some(popover_bounds());

        // 先在卡片附近判一次「在靠近」—— 这同时把历史最近距离记下来。
        assert_eq!(
            hover.off_target_action(true, Some(near_point()), bounds),
            OffTargetAction::ScheduleHide,
            "还没起表时应当排一次延迟隐藏"
        );
        hover.schedule_hide(cx, |_| {});
        assert!(hover.has_pending_hide());

        // 冲过头、往远走 → 不算靠近，且已排的计时器原样保留。
        assert_eq!(
            hover.off_target_action(true, Some(far_point()), bounds),
            OffTargetAction::KeepHidingDelay,
            "在走远时不该重启计时器"
        );
        assert!(
            hover.has_pending_hide(),
            "KeepHidingDelay 必须原样保留已排的计时器"
        );

        // 折返一点但仍比历史最近远 → 依旧不算靠近（否则「先冲过头再挪 10px」
        // 会被当成朝浮层去，计时器被无限往后推）。
        assert_eq!(
            hover.off_target_action(true, Some(back_off_point()), bounds),
            OffTargetAction::KeepHidingDelay,
            "比历史最近距离远超过 4px 就不是在靠近"
        );
    });
}

/// 真的在靠近 → 重启计时器，给指针时间赶到浮层上。
///
/// 重启的前提是「先清掉已排的计时器」，否则 [`HoverState::schedule_hide`]
/// 会因为已经排过而直接早退，等于没有重启。
#[gpui::test]
fn off_target_getting_closer_restarts_the_hide_timer(cx: &mut gpui::TestAppContext) {
    cx.update(|cx: &mut gpui::App| {
        let mut hover = HoverState::new();
        let bounds = Some(popover_bounds());

        hover.off_target_action(true, Some(near_point()), bounds);
        hover.schedule_hide(cx, |_| {});
        assert!(hover.has_pending_hide());

        assert_eq!(
            hover.off_target_action(true, Some(closer_point()), bounds),
            OffTargetAction::ScheduleHide,
            "在靠近时必须重启延迟隐藏"
        );
        assert!(
            !hover.has_pending_hide(),
            "返回 ScheduleHide 的同时要把旧计时器清掉，调用方才重排得动"
        );

        // 换锚点（新一轮查询）后最近距离要清零，否则上一张卡片的几何会把
        // 「靠近 / 走远」判错。
        hover.schedule_hide(cx, |_| {});
        hover.reset_closest_distance();
        assert_eq!(
            hover.off_target_action(true, Some(far_point()), bounds),
            OffTargetAction::ScheduleHide,
            "最近距离清零后第一次判「靠近」，与指针离得多远无关"
        );
    });
}

/// 判不了远近（没给指针位置，或浮层几何还没量出来）时不算在靠近。
///
/// 与 Zed `if let Some(mouse_position) = ...` 的默认值一致：走「不靠近」这一
/// 半边，于是已排的计时器照常跑完。
#[gpui::test]
fn unknown_geometry_is_not_getting_closer(cx: &mut gpui::TestAppContext) {
    cx.update(|cx: &mut gpui::App| {
        let mut hover = HoverState::new();
        assert_eq!(
            hover.off_target_action(true, None, Some(popover_bounds())),
            OffTargetAction::ScheduleHide,
            "没给指针位置时没起表 → 排一个（否则浮层没人收）"
        );
        hover.schedule_hide(cx, |_| {});
        assert_eq!(
            hover.off_target_action(true, Some(near_point()), None),
            OffTargetAction::KeepHidingDelay,
            "浮层几何缺失判不了远近 → 不算靠近"
        );
        assert_eq!(
            hover.off_target_action(false, None, None),
            OffTargetAction::CancelRequest,
            "没有卡片这一支与几何信息无关"
        );
    });
}
