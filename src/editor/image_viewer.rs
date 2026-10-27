use std::sync::Arc;

use gpui::prelude::*;
use gpui::{
    App, Bounds, Context, Element, ElementId, Entity, FocusHandle, GlobalElementId, ImageFormat,
    InspectorElementId, InteractiveElement, IntoElement, LayoutId, MouseDownEvent, ParentElement,
    Pixels, Render, ScrollDelta, ScrollWheelEvent, Style, Styled, Window, actions, checkerboard,
    div, img, px,
};

use crate::theme::ActiveTheme;

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 20.0;
const ZOOM_STEP: f32 = 1.1;
const SCROLL_LINE_MULTIPLIER: f32 = 20.0;
const BASE_SQUARE_SIZE: f32 = 8.0;

actions!(
    image_viewer,
    [ZoomIn, ZoomOut, ResetZoom, FitToView, ActualSize]
);

pub struct ImageViewer {
    image: Option<Arc<gpui::Image>>,
    image_size: Option<(u32, u32)>,
    zoom_level: f32,
    container_bounds: Option<Bounds<Pixels>>,
    fit_initialized: bool,
    focus_handle: FocusHandle,
}

impl ImageViewer {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let this = Self {
            image: None,
            image_size: None,
            zoom_level: 1.0,
            container_bounds: None,
            fit_initialized: false,
            focus_handle: cx.focus_handle(),
        };

        cx.on_release(|this, cx| {
            if let Some(image) = this.image.take() {
                image.remove_asset(cx);
            }
        })
        .detach();

        this
    }

    pub fn set_image(&mut self, image: Arc<gpui::Image>, size: (u32, u32), cx: &mut Context<Self>) {
        if let Some(old) = self.image.replace(image) {
            old.remove_asset(cx);
        }
        self.image_size = Some(size);
        cx.notify();
    }

    fn compute_fit_to_view_zoom(container_bounds: Bounds<Pixels>, image_size: (u32, u32)) -> f32 {
        let container_width: f32 = container_bounds.size.width.into();
        let container_height: f32 = container_bounds.size.height.into();
        let scale_x = container_width / image_size.0 as f32;
        let scale_y = container_height / image_size.1 as f32;
        scale_x.min(scale_y).min(1.0)
    }

    fn set_zoom(&mut self, new_zoom: f32, cx: &mut Context<Self>) {
        self.zoom_level = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);
        cx.notify();
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level * ZOOM_STEP, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level / ZOOM_STEP, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = 1.0;
        cx.notify();
    }

    fn fit_to_view(&mut self, _: &FitToView, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some((bounds, image_size)) = self.container_bounds.zip(self.image_size) {
            self.zoom_level = Self::compute_fit_to_view_zoom(bounds, image_size);
            cx.notify();
        }
    }

    fn actual_size(&mut self, _: &ActualSize, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = 1.0;
        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control || event.modifiers.platform {
            let delta: f32 = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels.y.into(),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_MULTIPLIER,
            };
            let zoom_factor = if delta > 0.0 {
                1.0 + delta.abs() * 0.01
            } else {
                1.0 / (1.0 + delta.abs() * 0.01)
            };
            self.set_zoom(self.zoom_level * zoom_factor, cx);
        }
    }

    fn handle_mouse_down(
        &mut self,
        _event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.focus_handle.focus(window, cx);
    }
}

crate::impl_focusable!(ImageViewer);

impl Render for ImageViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_loading = self.image.is_none();

        div()
            .id("image-viewer")
            .key_context("ImageViewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(cx.theme().ui.app_background)
            .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(Self::handle_mouse_down),
            )
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::fit_to_view))
            .on_action(cx.listener(Self::actual_size))
            .child(ImageContentElement {
                viewer: cx.entity(),
            })
            .when(is_loading, |this| {
                this.child(
                    div()
                        .absolute()
                        .size_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child("Loading image..."),
                )
            })
            .into_any_element()
    }
}

struct ImageContentElement {
    viewer: Entity<ImageViewer>,
}

impl Element for ImageContentElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<gpui::AnyElement>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    crate::impl_source_location_none!();

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = gpui::relative(1.0).into();
        style.size.height = gpui::relative(1.0).into();
        let layout_id = window.request_layout(style, [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let viewer = self.viewer.read(cx);
        let initial_zoom = if viewer.fit_initialized {
            None
        } else {
            viewer
                .image_size
                .map(|size| ImageViewer::compute_fit_to_view_zoom(bounds, size))
        };

        let zoom_level = initial_zoom.unwrap_or(viewer.zoom_level);

        let (scaled_width, scaled_height) = viewer
            .image_size
            .map(|(w, h)| (px(w as f32 * zoom_level), px(h as f32 * zoom_level)))
            .unwrap_or((px(0.0), px(0.0)));

        let center_x = bounds.size.width / 2.0;
        let center_y = bounds.size.height / 2.0;
        let left = center_x - scaled_width / 2.0;
        let top = center_y - scaled_height / 2.0;

        let image = viewer.image.clone();

        self.viewer.update(cx, |this, _| {
            this.container_bounds = Some(bounds);
            if initial_zoom.is_some() {
                this.fit_initialized = true;
            }
            if let Some(z) = initial_zoom {
                this.zoom_level = z;
            }
        });

        let mut image_content = div()
            .id(("image-viewer-content", self.viewer.entity_id()))
            .relative()
            .size_full()
            .child(
                div()
                    .absolute()
                    .left(left)
                    .top(top)
                    .w(scaled_width)
                    .h(scaled_height)
                    .when(image.is_some(), |this| {
                        this.bg(checkerboard(
                            cx.theme().ui.panel_background,
                            BASE_SQUARE_SIZE * zoom_level,
                        ))
                    })
                    .child(if let Some(image) = image {
                        img(image)
                            .id(("image-viewer-image", self.viewer.entity_id()))
                            .size_full()
                            .into_any_element()
                    } else {
                        div().size_full().into_any_element()
                    }),
            )
            .into_any_element();

        image_content.prepaint_as_root(bounds.origin, bounds.size.into(), window, cx);
        Some(image_content)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut (),
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if let Some(mut element) = prepaint.take() {
            element.paint(window, cx);
        }
    }
}

impl IntoElement for ImageContentElement {
    type Element = Self;
    fn into_element(self) -> Self::Element {
        self
    }
}

/// 可解码的图片扩展名（小写）。
///
/// 单一口径：图像标签页（[`is_image_file`]）、Markdown 图片解析与粘贴落盘
/// 都引用这一份，避免三处白名单各自漂移。`load_image_from_bytes` 能解码的
/// 格式必须全部列进来（`ppm`/`pnm` 由 `image::ImageFormat::Pnm` 覆盖）。
pub(crate) const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "tiff", "tif", "ico", "svg", "ppm", "pnm",
];

/// 这个路径是否应按「图片」处理：图像标签页用它决定打开方式，Markdown
/// 图片解析/粘贴落盘也用它做扩展名门禁（扩展名不区分大小写）。
pub fn is_image_file(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// 单张图片字节上限（解码器侧的硬门禁）。
///
/// Markdown 图片的元数据预筛 / 远程下载限流量都从这里取值，避免两处常量
/// 各自漂移导致"预筛放行、解码器拒绝"或反过来。
pub(crate) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

pub fn load_image_from_bytes(
    bytes: &[u8],
    path: &std::path::Path,
) -> anyhow::Result<(Arc<gpui::Image>, (u32, u32))> {
    // 关键安全/性能：限制解码炸弹。SVG 用受限 Options（禁用外部资源），
    // 位图先读尺寸，超限直接拒绝，避免 `image` 全量解码耗尽内存。
    const MAX_IMAGE_PIXELS: u64 = 8000 * 8000;
    if bytes.len() > MAX_IMAGE_BYTES {
        anyhow::bail!("图片过大（{} bytes），超过 20MB 限制", bytes.len());
    }
    if path.extension().and_then(|e| e.to_str()) == Some("svg") {
        let opt = usvg::Options {
            resources_dir: None,
            ..usvg::Options::default()
        };
        let tree = usvg::Tree::from_data(bytes, &opt)?;
        let size = tree.size();
        // SVG 逻辑尺寸过大同样拒绝（防 `<use>` 指数膨胀导致的渲染卡死）。
        if (size.width() as u64) * (size.height() as u64) > MAX_IMAGE_PIXELS
            || size.width() > 8000.0
            || size.height() > 8000.0
        {
            anyhow::bail!("SVG 尺寸过大，拒绝渲染");
        }
        let image = Arc::new(gpui::Image::from_bytes(ImageFormat::Svg, bytes.to_vec()));
        return Ok((image, (size.width() as u32, size.height() as u32)));
    }

    let format = image::guess_format(bytes)?;
    let gpui_format = match format {
        image::ImageFormat::Png => ImageFormat::Png,
        image::ImageFormat::Jpeg => ImageFormat::Jpeg,
        image::ImageFormat::WebP => ImageFormat::Webp,
        image::ImageFormat::Gif => ImageFormat::Gif,
        image::ImageFormat::Bmp => ImageFormat::Bmp,
        image::ImageFormat::Tiff => ImageFormat::Tiff,
        image::ImageFormat::Ico => ImageFormat::Ico,
        image::ImageFormat::Pnm => ImageFormat::Pnm,
        _ => anyhow::bail!("Unsupported image format: {:?}", format),
    };

    let reader = image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?;
    let (width, height) = reader.into_dimensions()?;
    // 位图像素总数同样设限，防解压炸弹耗尽内存。
    if (width as u64) * (height as u64) > MAX_IMAGE_PIXELS {
        anyhow::bail!("图片像素超限（{width}x{height}），拒绝渲染");
    }

    let image = Arc::new(gpui::Image::from_bytes(gpui_format, bytes.to_vec()));
    Ok((image, (width, height)))
}
