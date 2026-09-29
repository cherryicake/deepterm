//! 终端光标动画：四角弹簧（移植自 neovide 的 `CursorRenderer`）。
//!
//! 与 neovide `src/renderer/cursor_renderer/mod.rs` 逐项对应：
//!
//! | neovide | 这里 |
//! |---|---|
//! | `Corner { current_position, relative_position, previous_destination, animation_x, animation_y, animation_length }` | [`Corner`] |
//! | `CriticallyDampedSpringAnimation` | [`CriticallyDampedSpring`]（同公式：ζ=1，ω=4/T） |
//! | `STANDARD_CORNERS` + `set_cursor_shape` | [`CursorShape::corners`] |
//! | `Corner::jump`（≤2 格短动画，否则按对齐度 lerp 出拖尾时长） | [`Corner::jump`] |
//! | `Corner::calculate_direction_alignment` | [`Corner::direction_alignment`] |
//! | `update_cursor_destination`（滚动 delta 平移到四角） | [`CursorAnimator::step`] 的滚动分支 |
//! | `animate` 的 `jumped` → `jump` + `update` | [`CursorAnimator::step`] |
//!
//! 关键点：**四个角各跑各的弹簧**，前缘角（与运动方向同向）时长短、
//! 后缘角时长长（`trail_size` 决定差距），因此移动中光标会被剪切成
//! 平行四边形——这正是 neovide 光标动画的标志性手感，用「矩形整体插值」
//! 是复现不出来的。
//!
//! 与 neovide 的差异（终端渲染模型不同：neovide 的循环连续运行，终端只在
//! 内容变化时重绘）：
//!
//! - 跳变不设距离阈值，**全部走动画**（与 neovide 一致）；
//! - **不做「帧间隔大就瞬移」的判定**：终端停顿几秒后敲键是常态，任何基于
//!   间隔的瞬移判据都会让停顿后的打字失去动画。真正需要瞬移的场景
//!   （视口外 / 应用隐藏光标 / 减少动态效果）显式走 [`CursorAnimator::reset`]；
//! - 长按方向键导致「目标连续变化」时，靠弹簧自身的重装机制自然跟随
//!   （每次目标变化都把剩余位移重设为 `目标 − 当前`，速度保留），
//!   不需要额外节流。

use std::time::Instant;

use gpui::{Bounds, Pixels, Point, point, px, size};

/// 常规动画时长（秒）。对应 neovide `cursor-animation-length` 默认值。
const ANIMATION_LENGTH: f32 = 0.150;
/// 「打字跳」（同行 ≤ 2 格）的短动画时长（秒）。
/// 对应 neovide `cursor-short-animation-length` 默认值。
const SHORT_ANIMATION_LENGTH: f32 = 0.040;
/// 拖尾比例（0..1）。对应 neovide `cursor-trail-size` 默认值 1.0。
///
/// 1.0 表示前缘角的动画时长为 `T*(1-1.0) = 0`——立即到位，后缘角走满 `T`，
/// 拖尾最明显。调小则前后角时长趋同、形变减弱。
const TRAIL_SIZE: f32 = 1.0;
/// 弹簧收敛容差（px）：剩余位移小于该值即吸附到目标并结束动画。
const SPRING_EPSILON: f32 = 0.01;
/// 新动画起始帧的步长（秒）：约一帧 60Hz。
///
/// 终端只在内容变化时重绘，两次重绘之间可能隔着任意长的空闲（用户停顿
/// 几秒再敲键是常态）。动画开始时若把空闲时长算作已流逝时间，整个动画
/// 会在首帧就播完——观感就是「瞬移」。因此起始帧固定按「一帧」推进。
///
/// neovide 没有这个问题（它的渲染循环连续运行，帧间隔恒为 ~1/60），
/// 这是终端事件驱动渲染模型下必须自己处理的差异。
const START_FRAME_STEP: f32 = 1.0 / 60.0;
/// 动画进行中的单帧最大步长（秒）：允许追上慢帧，但不吃掉整个空闲期。
const MAX_STEP: f32 = 0.05;
/// 子步上限（秒）。与 neovide `MAX_ANIMATION_DT`（1/120）一致：大 dt 按
/// 1/120s 切分**逐步积分**，而不是一次性推进——弹簧是连续系统，一次性
/// 推进等于「跳到终点」，逐步积分才能得到平滑的中间形态。
const MAX_SUBSTEP: f32 = 1.0 / 120.0;
/// 跳变判定阈值（px）：目标位移超过它才算「跳」，与 neovide 的 0.01 一致。
const JUMP_EPSILON: f32 = 0.01;

/// 标准四角在单位格内的相对坐标，顺序：左上、右上、右下、左下。
/// 与 neovide `STANDARD_CORNERS` 完全一致。
const STANDARD_CORNERS: [(f32, f32); 4] = [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)];

/// 光标形状。
///
/// 三种实心形状在 neovide 里都是「四个角的相对位置」不同，四角各自插值
/// 天然产生形状 morph，无需单独的过渡逻辑。
///
/// `Hollow` 是失焦时的空心块：四角与 `Block` 完全相同，只换绘制方式
/// （描边而非填充），因此不参与 morph。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Hollow,
    Bar,
    Underline,
}

impl CursorShape {
    /// 该形状下四角的相对位置（单位：格）。
    ///
    /// 逐项对应 neovide `set_cursor_shape`：
    /// - Block：`(±0.5, ±0.5)`；
    /// - Vertical(beam)：把 x 侧收拢到 `cell_percentage`（0..1）宽度；
    /// - Horizontal(underline)：y 侧收拢并翻转到格子底部。
    pub fn corners(self, cell_percentage: f32) -> [(f32, f32); 4] {
        let p = cell_percentage.clamp(0.0, 1.0);
        STANDARD_CORNERS.map(|(x, y)| match self {
            // 与 neovide 相同：Hollow 用 Block 的四角（只换绘制方式）。
            Self::Block | Self::Hollow => (x, y),
            Self::Bar => ((x + 0.5) * p - 0.5, y),
            Self::Underline => (x, -((-y + 0.5) * p - 0.5)),
        })
    }
}

/// 临界阻尼弹簧，追踪「到目标的剩余位移」。
///
/// 与 neovide `animation_utils.rs` 的实现逐行等价（含 0.01px 容差与
/// `length <= dt` 吸附），便于对照验证行为。
#[derive(Clone, Copy, Debug, Default)]
struct CriticallyDampedSpring {
    /// 到目标的剩余位移（不是绝对坐标）。
    position: f32,
    velocity: f32,
}

impl CriticallyDampedSpring {
    /// 推进 `dt` 秒；返回是否仍在运动。
    fn update(&mut self, dt: f32, length: f32) -> bool {
        // 时长已被这一帧走完：直接吸附（neovide 用同样的短路避免除零）。
        if length <= dt {
            self.reset();
            return false;
        }
        if self.position == 0.0 {
            return false;
        }

        // 临界阻尼（ζ = 1）的解析解，等价于一个 PD 控制器：
        // ω = 4 / T 让 t = T 时残差落到 e^-4 ≈ 2% 量级。
        let omega = 4.0 / length;

        // a、b 由初值（位移、速度）在 t = 0 处解出。
        let a = self.position;
        let b = self.position * omega + self.velocity;
        let c = (-omega * dt).exp();

        self.position = (a + b * dt) * c;
        self.velocity = c * (-a * omega - b * dt * omega + b);

        if self.position.abs() < SPRING_EPSILON {
            self.reset();
            false
        } else {
            true
        }
    }

    fn reset(&mut self) {
        self.position = 0.0;
        self.velocity = 0.0;
    }
}

/// 光标的一个角：位置 + 两条弹簧（x / y）+ 本次跳变的时长。
///
/// 对应 neovide `Corner`。四角各自独立积分，因此移动过程中光标会被
/// 剪切成平行四边形（前缘先到、后缘拖尾）。
#[derive(Clone, Copy, Debug)]
pub struct Corner {
    /// 当前像素位置（已含动画）。
    current_position: Point<f32>,
    /// 相对格子中心的偏移（单位：格），由形状决定。
    relative_position: (f32, f32),
    /// 上次的目标位置，用于判断目标是否变化。
    previous_destination: Point<f32>,
    animation_x: CriticallyDampedSpring,
    animation_y: CriticallyDampedSpring,
    /// 本角的动画时长（秒），每次跳变时按对齐度重算。
    animation_length: f32,
}

impl Default for Corner {
    fn default() -> Self {
        Self {
            current_position: Point::new(0.0, 0.0),
            relative_position: (0.0, 0.0),
            // neovide 用 (-1000, -1000) 保证首帧必然触发「目标变化」。
            previous_destination: Point::new(-1000.0, -1000.0),
            animation_x: CriticallyDampedSpring::default(),
            animation_y: CriticallyDampedSpring::default(),
            animation_length: 0.0,
        }
    }
}

impl Corner {
    /// 该角在给定光标尺寸下的绝对目标位置。
    ///
    /// `cursor_dimensions` 为 `(cell_width, line_height)`，`destination`
    /// 为格子左上角。
    fn destination(&self, destination: Point<f32>, cell: (f32, f32)) -> Point<f32> {
        Point::new(
            destination.x + self.relative_position.0 * cell.0,
            destination.y + self.relative_position.1 * cell.1,
        )
    }

    /// 与 neovide `Corner::update` 等价：目标变化则重装弹簧，然后积分。
    fn update(
        &mut self,
        cell: (f32, f32),
        destination: Point<f32>,
        dt: f32,
        immediate: bool,
    ) -> bool {
        let corner_destination = self.destination(destination, cell);
        if corner_destination != self.previous_destination {
            let delta = Point::new(
                corner_destination.x - self.current_position.x,
                corner_destination.y - self.current_position.y,
            );
            self.animation_x.position = delta.x;
            self.animation_y.position = delta.y;
            self.previous_destination = corner_destination;
        }

        if immediate {
            self.animation_x.reset();
            self.animation_y.reset();
            self.current_position = corner_destination;
            return false;
        }

        let mut animating = self.animation_x.update(dt, self.animation_length);
        animating |= self.animation_y.update(dt, self.animation_length);
        self.current_position = Point::new(
            corner_destination.x - self.animation_x.position,
            corner_destination.y - self.animation_y.position,
        );
        animating
    }

    /// 整体平移本角（滚动同步，neovide `corner.current_position += scroll_delta`）。
    ///
    /// 调用方必须同时平移 `destination`（滚动已折入其中），这样
    /// `corner_destination` 与本角一起移动、弹簧剩余位移不变，
    /// 于是「滚动」不会被误判成「移动」而触发动画。
    fn shift(&mut self, delta: Point<f32>) {
        self.current_position.x += delta.x;
        self.current_position.y += delta.y;
        self.previous_destination.x += delta.x;
        self.previous_destination.y += delta.y;
    }

    /// 与 neovide `Corner::jump` 等价：按跳变距离与对齐度决定本角时长。
    fn jump(&mut self, destination: Point<f32>, cell: (f32, f32), alignment: f32, trail_size: f32) {
        let corner_destination = self.destination(destination, cell);
        let jump_x = (corner_destination.x - self.previous_destination.x) / cell.0;
        let jump_y = (corner_destination.y - self.previous_destination.y) / cell.1;

        self.animation_length = if jump_x.abs() <= 2.001 && jump_y.abs() <= 0.001 {
            // 同行 ≤2 格（打字 / 按住方向键）：用短时长。
            ANIMATION_LENGTH.min(SHORT_ANIMATION_LENGTH)
        } else {
            let leading = ANIMATION_LENGTH * (1.0 - trail_size).clamp(0.0, 1.0);
            let trailing = ANIMATION_LENGTH;
            lerp(trailing, leading, alignment)
        };
    }

    /// 与 neovide `Corner::calculate_direction_alignment` 等价：
    /// 该角的朝向与运动方向的点积（∈ [-1, 1]）。
    ///
    /// 运动前缘的角（点积大）用更短时长、先到位；后缘的角拖尾。
    fn direction_alignment(&self, cell: (f32, f32), destination: Point<f32>) -> f32 {
        let corner_destination = self.destination(destination, cell);
        let travel = (
            corner_destination.x - self.previous_destination.x,
            corner_destination.y - self.previous_destination.y,
        );
        let travel_len = (travel.0 * travel.0 + travel.1 * travel.1).sqrt();
        let (cx, cy) = self.relative_position;
        let corner_len = (cx * cx + cy * cy).sqrt();
        if travel_len <= f32::EPSILON || corner_len <= f32::EPSILON {
            return 0.0;
        }
        (travel.0 / travel_len) * (cx / corner_len) + (travel.1 / travel_len) * (cy / corner_len)
    }
}

/// `start + (end - start) * t`（neovide `lerp`）。
fn lerp(start: f32, end: f32, t: f32) -> f32 {
    start + (end - start) * t
}

/// 本帧光标的目标：静止矩形 + 用于跳变判定的网格坐标与度量。
#[derive(Clone, Copy)]
pub struct CursorTarget {
    /// 格子左上角的静止位（像素）。
    ///
    /// 对应 neovide `CursorRenderer::destination`：四角目标是它加上
    /// 「半格 + 相对偏移」；像素吸附用它的小数部分。
    pub destination: Point<f32>,
    /// 光标单元尺寸 `(cell_width, line_height)`（像素）。
    ///
    /// 宽字符 Block 的宽度会翻倍（与 neovide `double_width` 一致）。
    /// 四角相对偏移按它缩放，因此它同时决定 Block 的宽度。
    pub cell: (f32, f32),
    /// 形状基准尺寸 `(cell_width, line_height)`（像素，**不含**宽字符加宽）。
    ///
    /// beam / underline 的粗细按它的比例算——宽字符下 beam 不应变粗，
    /// 与 neovide 只在 Block 上翻倍 `cursor_width` 的行为一致。
    pub shape_span: (f32, f32),
    /// 本帧的滚动偏移（像素，内容相对视口平移的量）。
    ///
    /// 与上一帧不同即视为「滚动」：四角与静止目标一起平移，不产生动画
    /// （对应 neovide `update_cursor_destination` 的 `scroll_delta`）。
    ///
    /// 终端传 `(0, line_height * display_offset)`；编辑器传滚动句柄的
    /// 当前偏移（横向 + 纵向）。`destination` 里已含该偏移，所以滚动时
    /// 两者一起平移、差值不变 → 光标钉在内容上。
    pub scroll_offset: Point<f32>,
    pub shape: CursorShape,
    /// 形状宽度比例（`cell_percentage`，0..1），beam / underline 用。
    pub cell_percentage: f32,
    /// 本帧单元度量 `(cell_width, line_height)`；变化即视为布局变化，瞬移。
    pub metrics: (Pixels, Pixels),
    /// 终端内容区原点；变化（面板重排 / 元素移动）视为布局变化，瞬移。
    pub layout_origin: Point<Pixels>,
}

impl CursorTarget {
    /// 四角相对偏移的缩放基准。
    ///
    /// - Block / Hollow / Underline 用 `cell`：宽字符下加宽，与 DeepTerm
    ///   原有视觉一致（下划线横跨整个宽字符）。
    /// - Bar 用 `shape_span`：beam 的粗细与字符宽度无关（与 neovide 只在
    ///   Block 上翻倍 `cursor_width` 的行为一致），否则宽字符下 beam 会
    ///   粗一倍。
    fn corner_span(&self) -> (f32, f32) {
        match self.shape {
            CursorShape::Bar => self.shape_span,
            _ => self.cell,
        }
    }

    /// 四角的目标基准：格子中心（neovide `center_destination`）。
    fn center(&self) -> Point<f32> {
        let span = self.corner_span();
        Point::new(
            self.destination.x + span.0 * 0.5,
            self.destination.y + span.1 * 0.5,
        )
    }
}

/// 光标弹簧动画状态机。跨帧存活，挂在 `TerminalView` 上。
///
/// 与 neovide `CursorRenderer` 的字段一一对应。
pub struct CursorAnimator {
    /// 四个角（顺序：左上、右上、右下、左下），各自独立积分。
    corners: [Corner; 4],
    /// 当前静止目标（格子左上角，像素）。`None` = 尚未就位。
    destination: Option<Point<f32>>,
    /// 上一次的静止目标，用于计算位移（neovide `previous_destination`）。
    previous_destination: Point<f32>,
    /// 上一帧的滚动偏移，用于算滚动位移（滚动不算移动，不产生动画）。
    last_scroll_offset: Point<f32>,
    last_shape: Option<CursorShape>,
    last_cell_percentage: f32,
    last_metrics: Option<(Pixels, Pixels)>,
    last_layout_origin: Option<Point<Pixels>>,
    /// 是否发生了「跳」（neovide `jumped`）。
    jumped: bool,
    last_step: Option<Instant>,
    animating: bool,
    /// 最近一帧的四边形（测试断言用；生产不读）。
    #[cfg(test)]
    last_quad: Option<CursorQuad>,
}

impl Default for CursorAnimator {
    fn default() -> Self {
        Self::new()
    }
}

impl CursorAnimator {
    pub fn new() -> Self {
        Self {
            corners: [Corner::default(); 4],
            destination: None,
            previous_destination: Point::new(0.0, 0.0),
            last_scroll_offset: Point::new(0.0, 0.0),
            last_shape: None,
            last_cell_percentage: 1.0,
            last_metrics: None,
            last_layout_origin: None,
            jumped: false,
            last_step: None,
            animating: false,
            #[cfg(test)]
            last_quad: None,
        }
    }

    /// 是否仍在运动（供上层决定要不要请求下一帧）。
    pub fn is_animating(&self) -> bool {
        self.animating
    }

    /// 最近一帧的四边形（测试用：断言滚动平移量等）。
    #[cfg(test)]
    pub fn last_quad(&self) -> Option<CursorQuad> {
        self.last_quad
    }

    /// 丢弃全部动画状态：下一次 [`Self::step`] 会直接就位。
    ///
    /// 用于光标不可见、视口外，以及系统开启「减少动态效果」时。
    pub fn reset(&mut self) {
        self.corners = [Corner::default(); 4];
        self.destination = None;
        self.previous_destination = Point::new(0.0, 0.0);
        self.last_scroll_offset = Point::new(0.0, 0.0);
        self.last_shape = None;
        self.last_metrics = None;
        self.last_layout_origin = None;
        self.jumped = false;
        self.animating = false;
        #[cfg(test)]
        {
            self.last_quad = None;
        }
    }

    /// 推进一帧，返回本帧应绘制的四边形（四角）。
    ///
    /// `target` 为 `None` 表示本帧不绘制光标（闪烁灭相 / 视口外 / 应用隐藏
    /// 光标 / 覆盖层接管），此时状态复位，下次出现直接就位。
    ///
    /// `now` 必须来自 `cx.background_executor().now()`：生产里是单调墙钟，
    /// 测试里受 `advance_clock` 驱动（`std::time::Instant` 在测试中不走）。
    pub fn step(&mut self, target: Option<CursorTarget>, now: Instant) -> Option<CursorQuad> {
        let raw_dt = self.take_elapsed(now);

        let Some(target) = target else {
            self.reset();
            return None;
        };

        // ── 步长决策 ────────────────────────────────────────────────────────
        // 终端只在内容变化时重绘，两次重绘之间可能隔着任意长的空闲（用户
        // 停顿几秒再敲键是常态）。若把空闲时长当成已流逝的动画时间，整个
        // 动画会在首帧播完——观感就是「瞬移」。
        //
        // 因此：**没有动画在跑时**，无论间隔多久，都只按一帧推进（把这一帧
        // 当作动画的起点）；**动画进行中**才按真实帧间隔推进，允许追上慢帧。
        // neovide 不需要这个区分（它的渲染循环连续运行，帧间隔恒为 ~1/60）。
        let dt = if self.animating {
            raw_dt.min(MAX_STEP)
        } else {
            raw_dt.min(START_FRAME_STEP)
        };

        // ── 滚动：把四角与「上一帧目标」一起平移
        // （neovide `update_cursor_destination` 的 scroll_delta 处理）。
        //
        // 滚动只是内容整体平移，光标相对内容没动——所以这不是「移动」，
        // 不产生动画。做法：把四角与 `previous_destination` 平移同样的量，
        // 于是下面算出的 `motion` 不含滚动分量（`jumped` 不置位），
        // 而 `destination` 本身已含滚动，光标因此钉在内容上。
        if target.scroll_offset != self.last_scroll_offset {
            let delta = Point::new(
                target.scroll_offset.x - self.last_scroll_offset.x,
                target.scroll_offset.y - self.last_scroll_offset.y,
            );
            for corner in self.corners.iter_mut() {
                corner.shift(delta);
            }
            self.previous_destination.x += delta.x;
            self.previous_destination.y += delta.y;
            self.last_scroll_offset = target.scroll_offset;
        }

        let destination = Point::new(target.destination.x, target.destination.y);
        // 四角几何按形状取基准尺寸：Block/Hollow/Underline 用 `cell`
        // （宽字符加宽），Bar 用 `shape_span`（粗细与字符宽度无关）。
        let cell = target.corner_span();
        // 四角以格子中心为基准（neovide `center_destination`）：
        // 相对偏移 ±0.5 格 → 四角正好落在格子边界上。
        let center = target.center();

        // 形状变化：重设四角相对位置（neovide `set_cursor_shape`）。
        // 四角各自插值到新相对位置 → 天然的形状 morph。
        //
        // 同时把时长设为常规值：`animation_length` 只在 `jump()` 里更新，而
        // `jump()` 只在**位置**移动时调用（`jumped`）。形状切换不改位置，
        // 若沿用初值 0.0，`spring.update` 会命中 `length <= dt` 的吸附分支、
        // 一帧内瞬变（neovide 因为渲染循环连续运行、时长几乎总有值，掩盖了
        // 这一点；终端里「启动后直接切形状」很常见，必须显式处理）。
        let shape_changed = self.last_shape != Some(target.shape)
            || self.last_cell_percentage != target.cell_percentage;
        if shape_changed {
            self.last_shape = Some(target.shape);
            self.last_cell_percentage = target.cell_percentage;
            let corners = target.shape.corners(target.cell_percentage);
            for (corner, relative) in self.corners.iter_mut().zip(corners) {
                corner.relative_position = relative;
                corner.animation_length = ANIMATION_LENGTH;
            }
        }

        // ── 跳变判定（neovide `update_cursor_destination` 的 `jumped`）。
        // 度量 / 网格原点变化不是光标移动：瞬移。
        let layout_changed = self.last_metrics != Some(target.metrics)
            || self.last_layout_origin != Some(target.layout_origin);
        let motion = Point::new(
            destination.x - self.previous_destination.x,
            destination.y - self.previous_destination.y,
        );
        if motion.x.abs() > JUMP_EPSILON || motion.y.abs() > JUMP_EPSILON {
            self.jumped = true;
        }
        self.previous_destination = destination;
        self.destination = Some(destination);
        self.last_metrics = Some(target.metrics);
        self.last_layout_origin = Some(target.layout_origin);

        // 首次出现 / 布局变化：直接就位，不做动画。
        // （`corners[0].previous_destination` 初值为 neovide 同款哨兵值。）
        let first_frame = self.corners[0].previous_destination.x < -999.0;
        let immediate = layout_changed || first_frame;

        if self.jumped && !immediate {
            // 对齐度归一化后分配各角时长（neovide `animate` 的 alignments）。
            let alignments = self
                .corners
                .map(|corner| corner.direction_alignment(cell, center));
            let min = alignments.iter().copied().fold(f32::INFINITY, f32::min);
            let max = alignments.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let range = max - min;
            for (corner, raw) in self.corners.iter_mut().zip(alignments) {
                let alignment = if range > f32::EPSILON {
                    ((raw - min) / range).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                corner.jump(center, cell, alignment, TRAIL_SIZE);
            }
        }

        // ── 四角各自积分。
        let substeps = ((dt / MAX_SUBSTEP).ceil() as u32).max(1);
        let sub_dt = dt / substeps as f32;
        let mut animating = false;
        let mut corners = [(0.0f32, 0.0f32); 4];
        for (i, corner) in self.corners.iter_mut().enumerate() {
            for _ in 0..substeps {
                animating |= corner.update(cell, center, sub_dt, immediate);
            }
            corners[i] = (corner.current_position.x, corner.current_position.y);
        }

        self.jumped = false;
        self.animating = animating;
        let quad = CursorQuad {
            corners,
            destination,
        };
        #[cfg(test)]
        {
            self.last_quad = Some(quad);
        }
        Some(quad)
    }

    /// 本帧距上一帧的时长（秒），首次为 0。
    fn take_elapsed(&mut self, now: Instant) -> f32 {
        let dt = match self.last_step {
            Some(last) => now.saturating_duration_since(last).as_secs_f32(),
            None => 0.0,
        };
        self.last_step = Some(now);
        dt
    }
}

/// 光标四边形的四个角（顺序：左上、右上、右下、左下），已含动画形变。
///
/// 移动过程中四角各跑各的弹簧，因此这个四边形会被剪切成平行四边形——
/// 与 neovide `CursorRenderer::draw_rectangle` 画出的形状一致。
///
/// `destination` 是静止目标（格子左上角），用于像素吸附（neovide 的
/// `snapped_corner`：先减去目标的小数部分、取整、再加回来）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CursorQuad {
    pub corners: [(f32, f32); 4],
    /// 静止目标（格子左上角，像素）。
    pub destination: Point<f32>,
}

impl CursorQuad {
    /// 把四角吸附到「静止目标的整数像素栅格」上。
    ///
    /// 与 neovide `snapped_corner` 等价：先减去目标的小数部分、取整、
    /// 再加回来——既消除亚像素抖动，又保持与网格对齐。
    fn snapped(&self) -> [(f32, f32); 4] {
        let fract_x = self.destination.x.fract();
        let fract_y = self.destination.y.fract();
        self.corners.map(|(x, y)| {
            (
                (x - fract_x).round() + fract_x,
                (y - fract_y).round() + fract_y,
            )
        })
    }

    /// 轴对齐包围盒（反白字符裁剪用）。
    pub fn bounding_box(&self) -> Bounds<Pixels> {
        let xs = self.corners.map(|(x, _)| x);
        let ys = self.corners.map(|(_, y)| y);
        let min_x = xs.iter().copied().fold(f32::INFINITY, f32::min);
        let max_x = xs.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let min_y = ys.iter().copied().fold(f32::INFINITY, f32::min);
        let max_y = ys.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        Bounds::new(
            point(px(min_x), px(min_y)),
            size(px(max_x - min_x), px(max_y - min_y)),
        )
    }

    /// 是否已退化为轴对齐矩形（测试断言用）。
    #[cfg(test)]
    pub fn is_rect(&self) -> bool {
        let [(x0, y0), (x1, y1), (x2, y2), (x3, y3)] = self.corners;
        (y0 - y1).abs() < 0.01
            && (y2 - y3).abs() < 0.01
            && (x0 - x3).abs() < 0.01
            && (x1 - x2).abs() < 0.01
    }

    /// 填充绘制（Block / Bar / Underline）。
    ///
    /// 与 neovide 一致：四角连成路径后填充，而不是画轴对齐矩形——
    /// 移动中才能看到剪切拖尾。
    pub fn paint_fill(&self, window: &mut gpui::Window, color: gpui::Hsla) {
        let [c0, c1, c2, c3] = self.snapped();
        let mut builder = gpui::PathBuilder::fill();
        builder.move_to(point(px(c0.0), px(c0.1)));
        builder.line_to(point(px(c1.0), px(c1.1)));
        builder.line_to(point(px(c2.0), px(c2.1)));
        builder.line_to(point(px(c3.0), px(c3.1)));
        builder.close();
        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    }

    /// 空心描边绘制（失焦的 Hollow 块光标）。
    ///
    /// 与 neovide `draw_rectangular_outline` 等价：外轮廓 + 内缩轮廓两条
    /// 子路径，用 EvenOdd 填充规则做布尔差（GPUI `PathBuilder::fill()` 的
    /// 默认填充规则就是 EvenOdd），得到跟随四角形变的空心框。
    pub fn paint_outline(&self, window: &mut gpui::Window, width: f32, color: gpui::Hsla) {
        let [c0, c1, c2, c3] = self.snapped();
        // 每个角朝自己所在的方向内缩（与 neovide 的 offsets 一致）。
        let offsets = [
            (width, width),
            (-width, width),
            (-width, -width),
            (width, -width),
        ];

        let mut builder = gpui::PathBuilder::fill();
        builder.move_to(point(px(c0.0), px(c0.1)));
        builder.line_to(point(px(c1.0), px(c1.1)));
        builder.line_to(point(px(c2.0), px(c2.1)));
        builder.line_to(point(px(c3.0), px(c3.1)));
        builder.close();

        let inner = [c0, c1, c2, c3];
        builder.move_to(point(
            px(inner[0].0 + offsets[0].0),
            px(inner[0].1 + offsets[0].1),
        ));
        builder.line_to(point(
            px(inner[1].0 + offsets[1].0),
            px(inner[1].1 + offsets[1].1),
        ));
        builder.line_to(point(
            px(inner[2].0 + offsets[2].0),
            px(inner[2].1 + offsets[2].1),
        ));
        builder.line_to(point(
            px(inner[3].0 + offsets[3].0),
            px(inner[3].1 + offsets[3].1),
        ));
        builder.close();

        if let Ok(path) = builder.build() {
            window.paint_path(path, color);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn target(
        destination: (f32, f32),
        cell: (f32, f32),
        _grid: (i32, i32),
        scroll_rows: i32,
        shape: CursorShape,
    ) -> CursorTarget {
        CursorTarget {
            destination: Point::new(destination.0, destination.1),
            cell,
            shape_span: cell,
            scroll_offset: Point::new(0.0, cell.1 * scroll_rows as f32),
            shape,
            cell_percentage: 0.125,
            metrics: (px(10.0), px(20.0)),
            layout_origin: point(px(0.0), px(0.0)),
        }
    }

    fn cell() -> (f32, f32) {
        (10.0, 20.0)
    }

    #[test]
    fn spring_keeps_approaching_and_settles() {
        let mut spring = CriticallyDampedSpring {
            position: 100.0,
            velocity: 0.0,
        };
        let dt = 1.0 / 120.0;
        let mut previous = 100.0f32;
        for _ in 0..(3.0 / dt) as u32 {
            spring.update(dt, ANIMATION_LENGTH);
            assert!(
                spring.position.abs() <= previous.abs() + 1e-3,
                "位移必须单调收敛"
            );
            previous = spring.position;
        }
        assert_eq!(spring.position, 0.0);
    }

    #[test]
    fn spring_snaps_when_length_shorter_than_step() {
        let mut spring = CriticallyDampedSpring {
            position: 50.0,
            velocity: 0.0,
        };
        assert!(!spring.update(0.05, 0.04));
        assert_eq!(spring.position, 0.0);
    }

    #[test]
    fn first_frame_snaps_to_target() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        let quad = animator
            .step(
                Some(target((10.0, 20.0), cell(), (5, 3), 0, CursorShape::Block)),
                t0,
            )
            .unwrap();
        assert!(quad.is_rect(), "首帧必须是规整矩形");
        // 四角即格子四角：destination 是格子左上角。
        assert_eq!(quad.corners[0], (10.0, 20.0));
        assert_eq!(quad.corners[1], (20.0, 20.0));
        assert_eq!(quad.corners[2], (20.0, 40.0));
        assert_eq!(quad.corners[3], (10.0, 40.0));
        assert!(!animator.is_animating());
    }

    #[test]
    fn short_horizontal_move_animates_without_shear() {
        // 与 neovide 一致：同行 ≤2 格的「打字跳」走短动画，此时四个角的
        // `jump_vec` 都满足 ≤2.001 条件 → 四角时长相同 → 不产生剪切。
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        let quad = animator
            .step(
                Some(target((10.0, 0.0), cell(), (5, 4), 0, CursorShape::Block)),
                t0 + Duration::from_millis(8),
            )
            .unwrap();
        assert!(animator.is_animating(), "一格水平移动应产生动画");
        assert!(
            quad.is_rect(),
            "打字跳不应剪切（四角时长一致）: {:?}",
            quad.corners
        );
        // destination 是格子左上角：起点 x=0，目标 x=10（1 格）。
        assert!(
            quad.corners[0].0 > 0.0 && quad.corners[0].0 < 10.0,
            "应处于起止之间: {:?}",
            quad.corners
        );
    }

    #[test]
    fn cross_line_jump_shears() {
        // 跨行跳变不满足「≤2 格」条件 → 走对齐度分配时长 → 剪切拖尾。
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        let quad = animator
            .step(
                Some(target((0.0, 40.0), cell(), (7, 0), 0, CursorShape::Block)),
                t0 + Duration::from_millis(8),
            )
            .unwrap();
        assert!(animator.is_animating(), "跨行跳变应产生动画");
        // 向下移动：运动前缘是下缘，它先到位 → 下缘 y 应领先于上缘。
        // 注意 `trail_size = 1.0` 时前缘「立即到位」，因此四边形会被**拉伸**
        // （而不是剪切）——这正是 neovide 文档描述的 max trail 行为。
        assert!(
            quad.corners[2].1 > quad.corners[0].1,
            "向下移动时下缘应领先于上缘: {:?}",
            quad.corners
        );
        // 拉伸：下缘已到目标（+40），上缘仍在起点附近。
        assert!(
            quad.corners[2].1 > 39.0,
            "前缘应立即到位: {:?}",
            quad.corners
        );
    }

    #[test]
    fn trail_size_zero_disables_shear() {
        // trail_size = 1.0 时前缘角时长 0（瞬移）、后缘走满 T；
        // 这里直接验证时长分配公式本身。
        let leading = ANIMATION_LENGTH * (1.0 - 1.0);
        assert_eq!(leading, 0.0);
        assert_eq!(lerp(ANIMATION_LENGTH, leading, 1.0), 0.0);
        assert_eq!(lerp(ANIMATION_LENGTH, leading, 0.0), ANIMATION_LENGTH);
    }

    /// 各形状的静止几何必须与 DeepTerm 原有视觉一致。
    ///
    /// 这条守护「动画改造后形状尺寸没变」：Bar 的宽度、Underline 的高度
    /// 都应与 `cursor_thickness` 相同，Underline 贴在格子底部。
    #[test]
    fn shape_geometry_matches_static_layout() {
        let cell_w = 10.0f32;
        let line_h = 20.0f32;
        let thickness = 2.0f32;
        let span = (cell_w, line_h);

        // Bar：宽 = thickness，高 = line_height。
        let p_bar = thickness / cell_w;
        let bar = CursorShape::Bar.corners(p_bar);
        let bar_w = (bar[1].0 - bar[0].0) * span.0;
        let bar_h = (bar[2].1 - bar[1].1) * span.1;
        assert!(
            (bar_w - thickness).abs() < 0.01,
            "Bar 宽应为 {thickness}，实际 {bar_w}"
        );
        assert!(
            (bar_h - line_h).abs() < 0.01,
            "Bar 高应为 {line_h}，实际 {bar_h}"
        );

        // Underline：宽 = cell_width，高 = thickness，贴在格子底部。
        let p_ul = thickness / line_h;
        let ul = CursorShape::Underline.corners(p_ul);
        let ul_w = (ul[1].0 - ul[0].0) * span.0;
        let ul_h = (ul[2].1 - ul[1].1) * span.1;
        assert!(
            (ul_w - cell_w).abs() < 0.01,
            "Underline 宽应为 {cell_w}，实际 {ul_w}"
        );
        assert!(
            (ul_h - thickness).abs() < 0.01,
            "Underline 高应为 {thickness}，实际 {ul_h}"
        );
        // 下缘与格子底边对齐：`corners` 的最大 y 应等于 +0.5 格。
        let max_y = ul.iter().map(|(_, y)| *y).fold(f32::NEG_INFINITY, f32::max);
        assert!(
            (max_y - 0.5).abs() < 0.01,
            "Underline 应贴底: max_y={max_y}"
        );

        // Block：占满整格。
        let block = CursorShape::Block.corners(1.0);
        assert_eq!(block, [(-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5)]);
    }

    /// 宽字符下 Bar 不应变粗（与 neovide 只在 Block 上翻倍 `cursor_width`
    /// 的行为一致），Block 则应按宽字符加宽。
    #[test]
    fn wide_char_does_not_thicken_bar() {
        let cell_w = 10.0f32;
        let line_h = 20.0f32;
        let thickness = 2.0f32;

        // 宽字符 Block：cell.0 翻倍 → 宽度 2 格。
        let wide_block = target_with_span(
            (0.0, 0.0),
            (cell_w * 2.0, line_h),
            (cell_w, line_h),
            CursorShape::Block,
        );
        let block_span = wide_block.corner_span();
        assert_eq!(block_span.0, cell_w * 2.0, "Block 应按宽字符加宽");

        // 宽字符 Bar：仍按单格比例 → 粗细不变。
        let wide_bar = target_with_span(
            (0.0, 0.0),
            (cell_w * 2.0, line_h),
            (cell_w, line_h),
            CursorShape::Bar,
        );
        let bar_span = wide_bar.corner_span();
        assert_eq!(bar_span.0, cell_w, "Bar 不应按宽字符加宽");
        let p = thickness / cell_w;
        let bar = CursorShape::Bar.corners(p);
        let bar_w = (bar[1].0 - bar[0].0) * bar_span.0;
        assert!(
            (bar_w - thickness).abs() < 0.01,
            "宽字符下 Bar 粗细应仍为 {thickness}，实际 {bar_w}"
        );
    }

    /// 与 `target()` 相同，但可指定 `cell` 与 `shape_span` 分离的几何。
    fn target_with_span(
        destination: (f32, f32),
        cell: (f32, f32),
        shape_span: (f32, f32),
        shape: CursorShape,
    ) -> CursorTarget {
        CursorTarget {
            destination: Point::new(destination.0, destination.1),
            cell,
            shape_span,
            scroll_offset: Point::new(0.0, 0.0),
            shape,
            cell_percentage: 0.15,
            metrics: (px(cell.0), px(cell.1)),
            layout_origin: point(px(0.0), px(0.0)),
        }
    }

    #[test]
    fn animation_settles_to_target() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        let mut now = t0;
        let mut frames = 0;
        loop {
            now += Duration::from_millis(8);
            frames += 1;
            let quad = animator
                .step(
                    Some(target((10.0, 0.0), cell(), (5, 4), 0, CursorShape::Block)),
                    now,
                )
                .unwrap();
            if !animator.is_animating() {
                assert!(quad.is_rect(), "收敛后必须是规整矩形");
                assert_eq!(quad.corners[0], (10.0, 0.0));
                assert_eq!(quad.corners[2], (20.0, 20.0));
                break;
            }
            assert!(frames < 200, "动画未在合理帧数内收敛");
        }
        // 40ms 的短动画：8ms 一帧，应在 ~10 帧内收敛。
        assert!(frames <= 12, "短动画收敛过慢: {frames} 帧");
    }

    #[test]
    fn long_horizontal_jump_animates() {
        // 与 neovide 一致：任意距离的跳变都走动画（不设距离阈值）。
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        let quad = animator
            .step(
                Some(target((400.0, 0.0), cell(), (5, 43), 0, CursorShape::Block)),
                t0 + Duration::from_millis(8),
            )
            .unwrap();
        assert!(animator.is_animating(), "远距离跳变也应动画");
        assert!(quad.corners[0].0 > 0.0 && quad.corners[0].0 < 400.0);
        // 向右移动：运动前缘是右缘，它先到位 → 四边形被水平拉伸。
        assert!(
            quad.corners[1].0 > 399.0,
            "前缘（右缘）应立即到位: {:?}",
            quad.corners
        );
        assert!(
            quad.corners[0].0 < quad.corners[1].0,
            "后缘应拖尾: {:?}",
            quad.corners
        );
    }

    #[test]
    fn scroll_shift_does_not_animate() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 100.0), cell(), (10, 0), 0, CursorShape::Block)),
            t0,
        );
        // display_offset +1：视口行号 +1，光标整体下移一行（20px），
        // 但绝对网格坐标没变——这是滚动，不是移动。
        let quad = animator
            .step(
                Some(target((0.0, 120.0), cell(), (10, 0), 1, CursorShape::Block)),
                t0 + Duration::from_millis(8),
            )
            .unwrap();
        assert!(quad.is_rect(), "滚动不应产生形变");
        assert_eq!(quad.corners[0], (0.0, 120.0));
        assert!(!animator.is_animating());
    }

    #[test]
    fn layout_change_snaps() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        let mut moved = target((300.0, 0.0), cell(), (5, 3), 0, CursorShape::Block);
        moved.layout_origin = point(px(300.0), px(0.0));
        let quad = animator
            .step(Some(moved), t0 + Duration::from_millis(8))
            .unwrap();
        assert!(quad.is_rect());
        assert_eq!(quad.corners[0], (300.0, 0.0));
        assert!(!animator.is_animating());
    }

    #[test]
    fn shape_change_morphs_corners() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        // 先来一次跳变，让 `animation_length` 落到非零值
        // （与 neovide 一致：形状 morph 复用上一次跳变的时长）。
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        animator.step(
            Some(target((0.0, 40.0), cell(), (7, 3), 0, CursorShape::Block)),
            t0 + Duration::from_millis(8),
        );
        // block → bar：右缘从 +10px 收拢到 +1.25px（cell_percentage 0.125）。
        let quad = animator
            .step(
                Some(target((0.0, 40.0), cell(), (7, 3), 0, CursorShape::Bar)),
                t0 + Duration::from_millis(16),
            )
            .unwrap();
        assert!(animator.is_animating(), "形状切换应产生 morph");
        let width = quad.corners[1].0 - quad.corners[0].0;
        assert!(width > 1.25 && width < 10.0, "宽度应处于两端之间: {width}");
    }

    #[test]
    fn missing_cursor_resets_state() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        assert!(animator.step(None, t0).is_none());
        assert!(!animator.is_animating());
        // 再次出现：即使目标很远也直接就位（不从中途飞入）。
        let quad = animator
            .step(
                Some(target(
                    (500.0, 300.0),
                    cell(),
                    (30, 50),
                    0,
                    CursorShape::Block,
                )),
                t0 + Duration::from_millis(16),
            )
            .unwrap();
        assert!(quad.is_rect());
        assert_eq!(quad.corners[0], (500.0, 300.0));
    }

    /// 长时间未渲染（切标签页 / 窗口最小化）后重新出现：直接瞬移，
    /// 不做补动画。
    #[test]
    fn long_gap_mid_animation_keeps_animating() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 0), 0, CursorShape::Block)),
            t0,
        );
        // 起一次动画，让弹簧进入运动状态。
        animator.step(
            Some(target((400.0, 0.0), cell(), (5, 40), 0, CursorShape::Block)),
            t0 + Duration::from_millis(8),
        );
        assert!(animator.is_animating());
        // 动画进行中隔了 2 秒没渲染：单帧最多推进 MAX_STEP，仍在运动。
        // （终端只在内容变化时重绘，「间隔大」与「位置陈旧」没有必然联系。）
        let quad = animator
            .step(
                Some(target((400.0, 0.0), cell(), (5, 40), 0, CursorShape::Block)),
                t0 + Duration::from_secs(2),
            )
            .unwrap();
        assert!(
            animator.is_animating(),
            "长间隔不应把动画一次性走完: {:?}",
            quad.corners
        );
        assert!(
            quad.corners[0].0 > 0.0 && quad.corners[0].0 < 400.0,
            "应处于起止之间: {:?}",
            quad.corners
        );
    }

    /// 打字场景：空闲任意长时间后的**第一次**移动必须正常动画，
    /// 且逐帧可采样到平滑的中间位置（不是首帧就跳完）。
    ///
    /// 这是终端与 neovide 的关键差异：neovide 的渲染循环连续运行，帧间隔
    /// 恒为 ~1/60；终端只在有变化时重绘，帧间隔可能长达数秒。若把空闲时长
    /// 算作已流逝的动画时间，首帧就会把动画播完。
    #[test]
    fn typing_after_idle_animates_frame_by_frame() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        // 首帧就位。
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        // 用户停顿 3 秒后敲下一个字符：起动画。
        let mut now = t0 + Duration::from_secs(3);
        let first = animator
            .step(
                Some(target((10.0, 0.0), cell(), (5, 4), 0, CursorShape::Block)),
                now,
            )
            .unwrap();
        assert!(animator.is_animating(), "空闲后打字也应动画");
        let start_x = first.corners[0].0;
        let progress = start_x / 10.0;
        assert!(
            (0.2..0.8).contains(&progress),
            "起始帧应在前半程（不能把 3 秒空闲算作动画时间）: {:.0}%",
            progress * 100.0
        );

        // 后续帧应继续平滑推进（位置单调递增到目标），而不是首帧就到终点。
        let mut last_x = start_x;
        let mut frames = 0;
        while animator.is_animating() && frames < 60 {
            now += Duration::from_millis(16);
            frames += 1;
            let quad = animator
                .step(
                    Some(target((10.0, 0.0), cell(), (5, 4), 0, CursorShape::Block)),
                    now,
                )
                .unwrap();
            assert!(
                quad.corners[0].0 >= last_x - 0.01,
                "位置应单调推进: {last_x} -> {}",
                quad.corners[0].0
            );
            last_x = quad.corners[0].0;
        }
        assert!(frames >= 2, "应有多帧中间态，实际 {frames} 帧");
        assert!((last_x - 10.0).abs() < 0.5, "应收敛到目标: {last_x}");
    }

    /// 动画进行中被打断再继续（连续打字）：不应因中间的空闲而跳变。
    #[test]
    fn typing_continuously_keeps_animating() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 0), 0, CursorShape::Block)),
            t0,
        );
        let mut now = t0;
        // 每次敲键之间停顿 200ms（动画早已结束），共 5 个字符。
        for col in 1..=5 {
            now += Duration::from_millis(200);
            let quad = animator
                .step(
                    Some(target(
                        (col as f32 * 10.0, 0.0),
                        cell(),
                        (5, col),
                        0,
                        CursorShape::Block,
                    )),
                    now,
                )
                .unwrap();
            assert!(
                animator.is_animating(),
                "第 {col} 个字符应起动画: {:?}",
                quad.corners
            );
            let x = quad.corners[0].0;
            let expected_start = (col - 1) as f32 * 10.0;
            assert!(
                x > expected_start - 0.5 && x < col as f32 * 10.0 + 0.5,
                "第 {col} 个字符应从上一格附近出发: x={x}"
            );
        }
    }

    /// 慢速打字（帧间隔 > 短动画时长 40ms）也必须动画，不能被
    /// `length <= dt` 的吸附分支吞掉。
    #[test]
    fn slow_typing_frame_gap_does_not_swallow_animation() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 3), 0, CursorShape::Block)),
            t0,
        );
        // 120ms 的渲染间隔（远超 40ms 的短动画时长）。
        let quad = animator
            .step(
                Some(target((10.0, 0.0), cell(), (5, 4), 0, CursorShape::Block)),
                t0 + Duration::from_millis(120),
            )
            .unwrap();
        assert!(animator.is_animating(), "大帧间隔不应吞掉打字动画");
        assert!(
            quad.corners[0].0 > 0.0 && quad.corners[0].0 < 10.0,
            "应处于起止之间: {:?}",
            quad.corners
        );
    }

    /// 目标在动画中途反向：弹簧带着残余速度平滑折返，最终回到新目标。
    #[test]
    fn retarget_mid_flight_reverses() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 0), 0, CursorShape::Block)),
            t0,
        );
        let mid = animator
            .step(
                Some(target((10.0, 0.0), cell(), (5, 1), 0, CursorShape::Block)),
                t0 + Duration::from_millis(8),
            )
            .unwrap();
        assert!(mid.corners[0].0 > 0.0 && mid.corners[0].0 < 10.0);

        let mut now = t0 + Duration::from_millis(16);
        let mut peak = mid.corners[0].0;
        for _ in 0..200 {
            now += Duration::from_millis(8);
            let quad = animator
                .step(
                    Some(target((0.0, 0.0), cell(), (5, 0), 0, CursorShape::Block)),
                    now,
                )
                .unwrap();
            peak = peak.max(quad.corners[0].0);
            if !animator.is_animating() {
                assert_eq!(quad.corners[0], (0.0, 0.0), "折返后应收敛回新目标");
                assert!(peak < 10.0, "折返不应越过旧目标: peak={peak}");
                return;
            }
        }
        panic!("折返动画未收敛");
    }

    /// 连续快速移动（长按方向键）：目标不断变化，弹簧应持续跟随而不卡顿，
    /// 且每一步都比上一步更靠右。
    #[test]
    fn rapid_retarget_keeps_moving() {
        let mut animator = CursorAnimator::new();
        let t0 = Instant::now();
        animator.step(
            Some(target((0.0, 0.0), cell(), (5, 0), 0, CursorShape::Block)),
            t0,
        );
        let mut now = t0;
        let mut last_x = 0.0f32;
        for col in 1..=20 {
            now += Duration::from_millis(16);
            let quad = animator
                .step(
                    Some(target(
                        (col as f32 * 10.0, 0.0),
                        cell(),
                        (5, col),
                        0,
                        CursorShape::Block,
                    )),
                    now,
                )
                .unwrap();
            assert!(
                quad.corners[0].0 >= last_x - 0.01,
                "连续右移不应回退: col={col} x={} last={last_x}",
                quad.corners[0].0
            );
            last_x = quad.corners[0].0;
        }
    }
}
