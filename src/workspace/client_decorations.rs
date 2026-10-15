use gpui::prelude::FluentBuilder;
use gpui::{
    Bounds, CursorStyle, Decorations, HitboxBehavior, Hsla, InteractiveElement, MouseButton,
    ParentElement, Point, ResizeEdge, Stateful, Styled, Window, canvas, div, point, px, size,
    transparent_black,
};

use crate::theme::ActiveTheme;
use crate::theme::{CLIENT_SIDE_DECORATION_ROUNDING, CLIENT_SIDE_DECORATION_SHADOW};

pub fn client_side_decorations(
    element: impl gpui::IntoElement,
    window: &mut Window,
    cx: &mut gpui::App,
) -> Stateful<gpui::Div> {
    const BORDER_SIZE: gpui::Pixels = px(1.0);
    let decorations = window.window_decorations();
    let tiling = match decorations {
        Decorations::Server => gpui::Tiling::default(),
        Decorations::Client { tiling } => tiling,
    };

    match decorations {
        Decorations::Client { .. } => window.set_client_inset(CLIENT_SIDE_DECORATION_SHADOW),
        Decorations::Server => window.set_client_inset(px(0.0)),
    }

    div()
        .id("window-backdrop")
        .bg(transparent_black())
        .map(|div| match decorations {
            Decorations::Server => div,
            Decorations::Client { .. } => div
                .when(!(tiling.top || tiling.right), |div| {
                    div.rounded_tr(CLIENT_SIDE_DECORATION_ROUNDING)
                })
                .when(!(tiling.top || tiling.left), |div| {
                    div.rounded_tl(CLIENT_SIDE_DECORATION_ROUNDING)
                })
                .when(!(tiling.bottom || tiling.right), |div| {
                    div.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING)
                })
                .when(!(tiling.bottom || tiling.left), |div| {
                    div.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING)
                })
                // 四边内边距一律按 tiling 收放。这块留白是给窗口阴影用的透明区，
                // 窗口一旦贴边（最大化时 tiling 四边皆 true），留在那儿的 padding
                // 就变屏幕上一条肉眼可见的缝——实测左边和上边各 10px，而右边和下边
                // 本来就带着 when，所以看不出问题。
                //
                // pt/pl 过去是无条件保留的，旧注释给的理由是「怕内容上移后标题栏
                // 落进顶部缩放热区」。这个担心是多余的：resize_edge() 里判定顶边
                // 缩放的第一个条件就是 `!tiling.top`，左边同理 `!tiling.left`，
                // 贴边时这两条分支根本不会返回，留着 padding 保护不了任何东西。
                .when(!tiling.top, |div| div.pt(CLIENT_SIDE_DECORATION_SHADOW))
                .when(!tiling.bottom, |div| div.pb(CLIENT_SIDE_DECORATION_SHADOW))
                .when(!tiling.left, |div| div.pl(CLIENT_SIDE_DECORATION_SHADOW))
                .when(!tiling.right, |div| div.pr(CLIENT_SIDE_DECORATION_SHADOW))
                .on_mouse_down(MouseButton::Left, move |e, window, _| {
                    let size = window.window_bounds().get_bounds().size;
                    let pos = e.position;

                    let edge = match resize_edge(pos, CLIENT_SIDE_DECORATION_SHADOW, size, tiling) {
                        Some(value) => value,
                        None => return,
                    };

                    window.start_window_resize(edge);
                }),
        })
        .size_full()
        .child(
            div()
                .cursor(CursorStyle::Arrow)
                .map(|div| match decorations {
                    Decorations::Server => div,
                    Decorations::Client { .. } => div
                        .border_color(cx.theme().ui.border)
                        .when(!(tiling.top || tiling.right), |div| {
                            div.rounded_tr(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!(tiling.top || tiling.left), |div| {
                            div.rounded_tl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!(tiling.bottom || tiling.right), |div| {
                            div.rounded_br(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!(tiling.bottom || tiling.left), |div| {
                            div.rounded_bl(CLIENT_SIDE_DECORATION_ROUNDING)
                        })
                        .when(!tiling.top, |div| div.border_t(BORDER_SIZE))
                        .when(!tiling.bottom, |div| div.border_b(BORDER_SIZE))
                        .when(!tiling.left, |div| div.border_l(BORDER_SIZE))
                        .when(!tiling.right, |div| div.border_r(BORDER_SIZE))
                        .when(!tiling.is_tiled(), |div| {
                            div.shadow(vec![gpui::BoxShadow {
                                color: Hsla {
                                    h: 0.,
                                    s: 0.,
                                    l: 0.,
                                    a: 0.4,
                                },
                                blur_radius: CLIENT_SIDE_DECORATION_SHADOW / 2.,
                                spread_radius: px(0.),
                                inset: false,
                                offset: point(px(0.0), px(0.0)),
                            }])
                        }),
                })
                .on_mouse_move(|_e, _, cx| {
                    cx.stop_propagation();
                })
                .size_full()
                .child(element),
        )
        .map(|div| match decorations {
            Decorations::Server => div,
            Decorations::Client { tiling, .. } => div.child(
                canvas(
                    |_bounds, window, _| {
                        window.insert_hitbox(
                            Bounds::new(
                                point(px(0.0), px(0.0)),
                                window.window_bounds().get_bounds().size,
                            ),
                            HitboxBehavior::Normal,
                        )
                    },
                    move |_bounds, hitbox, window, _cx| {
                        let mouse = window.mouse_position();
                        let size = window.window_bounds().get_bounds().size;
                        let Some(edge) =
                            resize_edge(mouse, CLIENT_SIDE_DECORATION_SHADOW, size, tiling)
                        else {
                            return;
                        };
                        window.set_cursor_style(
                            match edge {
                                ResizeEdge::Top | ResizeEdge::Bottom => CursorStyle::ResizeUpDown,
                                ResizeEdge::Left | ResizeEdge::Right => {
                                    CursorStyle::ResizeLeftRight
                                }
                                ResizeEdge::TopLeft | ResizeEdge::BottomRight => {
                                    CursorStyle::ResizeUpLeftDownRight
                                }
                                ResizeEdge::TopRight | ResizeEdge::BottomLeft => {
                                    CursorStyle::ResizeUpRightDownLeft
                                }
                            },
                            &hitbox,
                        );
                    },
                )
                .size_full()
                .absolute(),
            ),
        })
}

fn resize_edge(
    pos: Point<gpui::Pixels>,
    shadow_size: gpui::Pixels,
    window_size: gpui::Size<gpui::Pixels>,
    tiling: gpui::Tiling,
) -> Option<ResizeEdge> {
    let bounds = Bounds::new(Point::default(), window_size).inset(shadow_size * 1.5);
    if bounds.contains(&pos) {
        return None;
    }

    let corner_size = size(shadow_size * 1.5, shadow_size * 1.5);
    let top_left_bounds = Bounds::new(Point::new(px(0.), px(0.)), corner_size);
    if !tiling.top && top_left_bounds.contains(&pos) {
        return Some(ResizeEdge::TopLeft);
    }

    let top_right_bounds = Bounds::new(
        Point::new(window_size.width - corner_size.width, px(0.)),
        corner_size,
    );
    if !tiling.top && top_right_bounds.contains(&pos) {
        return Some(ResizeEdge::TopRight);
    }

    let bottom_left_bounds = Bounds::new(
        Point::new(px(0.), window_size.height - corner_size.height),
        corner_size,
    );
    if !tiling.bottom && bottom_left_bounds.contains(&pos) {
        return Some(ResizeEdge::BottomLeft);
    }

    let bottom_right_bounds = Bounds::new(
        Point::new(
            window_size.width - corner_size.width,
            window_size.height - corner_size.height,
        ),
        corner_size,
    );
    if !tiling.bottom && bottom_right_bounds.contains(&pos) {
        return Some(ResizeEdge::BottomRight);
    }

    if !tiling.top && pos.y < shadow_size {
        Some(ResizeEdge::Top)
    } else if !tiling.bottom && pos.y > window_size.height - shadow_size {
        Some(ResizeEdge::Bottom)
    } else if !tiling.left && pos.x < shadow_size {
        Some(ResizeEdge::Left)
    } else if !tiling.right && pos.x > window_size.width - shadow_size {
        Some(ResizeEdge::Right)
    } else {
        None
    }
}
