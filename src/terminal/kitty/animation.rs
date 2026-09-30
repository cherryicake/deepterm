//! Kitty 图形协议的动画：帧模型、像素合成、定时推进。
//!
//! 规范：<https://sw.kovidgoyal.net/kitty/graphics-protocol/#animation>
//!
//! ## 帧编号
//!
//! 一帧动画挂在**已存在的图像**上。**帧 1 就是图像自身的基础数据**（"根帧"），
//! 不在这里存储；帧 `2..N` 是本模块的 `frames[i]`，即协议帧号 `i + 2`。这个
//! 约定贯穿全文件——`frame_index(protocol_number)` 负责换算。
//!
//! ## 为什么提前合成
//!
//! kitty 把帧存成 delta 矩形并在磁盘缓存里惰性合成。这里改成**每帧在加载时
//! 就合成成整幅图像大小的 RGBA 缓冲**：用一点内存换掉引用链的维护、合并与
//! 垃圾回收。帧数据与图像数据共用同一份字节上限（见 [`super::storage`]）。
//!
//! ## 合成必须统一成 RGBA
//!
//! 图像第一次参与动画合成时会被就地转成 RGBA，传输来的帧数据也先转 RGBA 再
//! 合成。不透明（alpha=255）源的 src-over 退化成拷贝，因此相对 kitty 分开的
//! RGB/RGBA 路径不丢信息。

/// 新建帧未指定 `z=` 时间隔时使用的默认值（kitty 的 `DEFAULT_GAP`）。
pub const DEFAULT_GAP_MS: u32 = 40;

/// 合成模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CompositionMode {
    /// src-over 混合（默认）。
    #[default]
    AlphaBlend,
    /// 直接覆盖。
    Overwrite,
}

/// 一块画布背景色（`Y=` 键），按 0xRRGGBBAA 解释。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Background {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Background {
    /// 从协议里的 u32 位模式构造。
    ///
    /// 键值按小端位转换成 u32，因此 `Y=4278190335`（0xff0000ff）是「不透明
    /// 红」——位模式里 r 在最低字节。这里按同一约定取各分量。
    pub fn from_bits(bits: u32) -> Self {
        Self {
            r: (bits & 0xFF) as u8,
            g: ((bits >> 8) & 0xFF) as u8,
            b: ((bits >> 16) & 0xFF) as u8,
            a: ((bits >> 24) & 0xFF) as u8,
        }
    }

    /// 是否全透明（全 0）。全透明时可以直接 `memset` 0。
    pub fn is_zero(self) -> bool {
        self.r == 0 && self.g == 0 && self.b == 0 && self.a == 0
    }
}

/// 播放状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    /// 不自动推进；帧只在客户端 `a=a c=N` 时改变（所有动画的初始状态）。
    #[default]
    Stopped,
    /// 自动推进，但走到最后一帧就停住等客户端继续送帧（`a=a s=2`）。
    Loading,
    /// 自动推进并循环（`a=a s=3`）。
    Running,
}

/// 一个动画帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// 已完整合成好的像素数据，恒为 `图像宽 * 图像高 * 4` 字节 RGBA。
    pub data: Vec<u8>,
    /// 本帧显示多少毫秒后推进。0 表示无间隔（播放时被跳过）。
    pub gap_ms: u32,
}

/// 单张图像的动画状态（由图像惰性创建）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Animation {
    /// 根帧之后的帧：`frames[i]` 对应协议帧号 `i + 2`。
    pub frames: Vec<Frame>,
    /// 根帧（帧 1）的间隔毫秒数。0 表示无间隔（播放时被跳过）。
    ///
    /// kitty 创建根帧时间隔为 0，客户端用 `a=a,r=1,z=N` 给它赋值。
    pub root_gap_ms: u32,
    /// 当前显示帧的**零基**下标：0 为根帧，`i >= 1` 为 `frames[i - 1]`。
    pub current_index: u32,
    /// 播放状态。
    pub state: State,
    /// 最多循环几轮（0 表示无限）。由 `a=a v=` 以 `v-1` 写入（协议的差一编码）。
    pub max_loops: u32,
    /// 播放开始以来已完成的轮数。
    pub current_loop: u32,
    /// 当前帧被显示时的毫秒时间戳（时钟由调用方选定，见 [`Animation::tick`]）。
    /// `None` 表示"尚未显示"，下一次 tick 会补上。
    pub frame_shown_at_ms: Option<u64>,
}

/// 协议帧号（1 起）→ [`Animation::frames`] 下标（0 起）。
///
/// 帧 1 是图像自身数据，不在 `frames` 里，因此帧 2 对应下标 0。
fn frame_index(number: u32) -> Option<usize> {
    usize::try_from(number.checked_sub(2)?).ok()
}

impl Animation {
    /// 含根帧在内的总帧数。
    pub fn frame_count(&self) -> u32 {
        // frames.len() 上限受 usize 限制；这里用饱和转换避免溢出恐慌。
        u32::try_from(self.frames.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1)
    }

    /// 零基下标 `index` 处的间隔。
    pub fn gap_at(&self, index: u32) -> u32 {
        if index == 0 {
            return self.root_gap_ms;
        }
        frame_index(index + 1)
            .and_then(|i| self.frames.get(i))
            .map(|f| f.gap_ms)
            .unwrap_or(0)
    }

    /// 设置零基下标 `index` 处的间隔。
    pub fn set_gap_at(&mut self, index: u32, gap_ms: u32) {
        if index == 0 {
            self.root_gap_ms = gap_ms;
            return;
        }
        if let Some(frame) = frame_index(index + 1).and_then(|i| self.frames.get_mut(i)) {
            frame.gap_ms = gap_ms;
        }
    }

    /// 所有间隔之和。
    ///
    /// 总时长为 0（每帧都无间隔）的动画永远不会推进；这同时是 [`Animation::tick`]
    /// 里"跳过无间隔帧"循环的终止保证。
    pub fn duration_ms(&self) -> u64 {
        let mut total = self.root_gap_ms as u64;
        for frame in &self.frames {
            total += frame.gap_ms as u64;
        }
        total
    }

    /// 本动画占用的帧数据字节数（计入图像存储上限）。
    pub fn frame_bytes(&self) -> usize {
        self.frames.iter().map(|f| f.data.len()).sum()
    }

    /// 取协议帧号 `number` 的帧数据。帧号非法或不存在时返回 `None`。
    pub fn frame(&self, number: u32) -> Option<&Frame> {
        frame_index(number).and_then(|i| self.frames.get(i))
    }

    /// 当前显示帧的零基下标对应的帧数据（根帧返回 `None`，它在图像自身里）。
    pub fn current_frame(&self) -> Option<&Frame> {
        if self.current_index == 0 {
            return None;
        }
        frame_index(self.current_index + 1).and_then(|i| self.frames.get(i))
    }

    /// 按 `now_ms` 推进到应显示的帧，并回报下一次帧变化距 `now_ms` 还有多少
    /// 毫秒。`None` 表示没有运行中的动画需要未来的 tick。
    ///
    /// `now_ms` 是调用方选定时钟上的单调时间戳；所有调用必须用同一时钟。
    /// 调用方是渲染层：每次构建帧时 tick 一次，并按返回值安排唤醒。
    ///
    /// 返回值 `(delay, content_changed)`：`content_changed` 为真表示**当前显示
    /// 的像素变了**，调用方需要让纹理缓存失效（代际号由 `storage` 侧负责）。
    pub fn tick(&mut self, now_ms: u64) -> (Option<u64>, bool) {
        // 以下门槛对齐 kitty 的 `image_is_animatable`。

        // 从未挂过动画状态（普通图像）。
        if self.frames.is_empty() {
            return (None, false);
        }
        // stopped 是每个动画的初始状态：帧只能由客户端驱动（`a=a c=N`）。
        if self.state == State::Stopped {
            return (None, false);
        }
        // 总时长为 0：每帧都无间隔，永远没有可显示的帧。
        if self.duration_ms() == 0 {
            return (None, false);
        }
        // 有限循环额度（`a=a v=N`）用尽后，播放永久停在上次那帧。
        if self.max_loops > 0 && self.current_loop >= self.max_loops {
            return (None, false);
        }

        // 本次播放（或当前帧被其他路径改过，如 `a=a c=N`）以来的首个 tick：
        // 视当前帧为"此刻刚显示"，它的间隔从此刻开始计时。
        let shown_at = match self.frame_shown_at_ms {
            // 未来时间戳说明调用方的时钟重启过：重新锚定，别傻等旧时间点。
            Some(at) if at <= now_ms => at,
            _ => now_ms,
        };
        self.frame_shown_at_ms = Some(shown_at);

        // 当前帧的间隔走完才换帧。**每次 tick 最多推进一帧、不追帧**——与 kitty
        // 一致：tick 跟不上间隔时动画变慢，而不是跳帧。
        let mut next_at = shown_at.saturating_add(self.gap_at(self.current_index) as u64);
        let mut changed = false;

        if now_ms >= next_at {
            let count = self.frame_count();
            let mut idx = self.current_index;
            // 这个循环只是跳过无间隔（gap=0）的帧——它们从不显示。上面的
            // duration_ms 门槛保证循环一定能找到可显示的帧。
            'advance: loop {
                let next = (idx + 1) % count;
                if next == 0 {
                    // 绕回根帧。loading 状态（`a=a s=2`）拒绝绕回：
                    // 停在最后一帧等客户端再送帧。
                    if self.state == State::Loading {
                        break 'advance;
                    }
                    // 每绕一圈算完成一轮；额度刚用尽就停在最后一帧。
                    self.current_loop += 1;
                    if self.max_loops > 0 && self.current_loop >= self.max_loops {
                        break 'advance;
                    }
                }
                idx = next;
                if self.gap_at(idx) != 0 {
                    break 'advance;
                }
            }

            if idx != self.current_index {
                // 显示选中的帧：重置它的间隔计时，并标记内容变化供渲染层失效纹理。
                self.current_index = idx;
                self.frame_shown_at_ms = Some(now_ms);
                changed = true;
                next_at = now_ms.saturating_add(self.gap_at(idx) as u64);
            }
        }

        // 安排下一次 tick。已停住的动画 `next_at` 落在过去，因而不会安排；
        // 它靠自己的触发条件唤醒（新帧到达，或 `a=a` 改状态）。
        if next_at > now_ms {
            (Some(next_at - now_ms), changed)
        } else {
            (None, changed)
        }
    }

    /// 是否存在需要定时器推进的动画（供渲染层快速判断）。
    pub fn is_animating(&self) -> bool {
        self.state != State::Stopped && !self.frames.is_empty() && self.duration_ms() > 0
    }
}

/// 用给定的背景色填充一块 RGBA 缓冲。
pub fn fill_background(data: &mut [u8], bg: Background) {
    if bg.is_zero() {
        data.fill(0);
        return;
    }
    for px in data.chunks_exact_mut(4) {
        px.copy_from_slice(&[bg.r, bg.g, bg.b, bg.a]);
    }
}

/// 把 `src` 矩形（`src_width × src_height` RGBA 像素）以左上角对齐到
/// `(x, y)` 合成到 `dst` 画布（`dst_width × dst_height` RGBA 像素）上。
///
/// 超出画布的部分**静默裁剪**，对齐 kitty 的 `a=f` 行为。
pub fn compose_rect(
    dst: &mut [u8],
    dst_width: u32,
    dst_height: u32,
    src: &[u8],
    src_width: u32,
    src_height: u32,
    x: u32,
    y: u32,
    mode: CompositionMode,
) {
    if x >= dst_width || y >= dst_height {
        return;
    }
    let width = (src_width.min(dst_width - x)) as usize;
    let height = (src_height.min(dst_height - y)) as usize;
    for row in 0..height {
        let dst_off = (((y as usize) + row) * dst_width as usize + x as usize) * 4;
        let src_off = row * src_width as usize * 4;
        compose_row(
            &mut dst[dst_off..dst_off + width * 4],
            &src[src_off..src_off + width * 4],
            mode,
        );
    }
}

/// 在两块**同宽**（stride 均为 `canvas_width`）的整幅 RGBA 画布之间合成一个
/// `width × height` 矩形：从 `src` 的 `(src_x, src_y)` 读到 `dst` 的 `(dst_x, dst_y)`。
///
/// 调用方必须保证两个矩形都**在界内**——`a=c` 把越界矩形报成错误而不是裁剪，
/// 与 `a=f` 不同。两个矩形不相交时，同帧自合成也是良定义的。
pub fn compose_canvas_rect(
    dst: &mut [u8],
    src: &[u8],
    canvas_width: u32,
    width: u32,
    height: u32,
    src_x: u32,
    src_y: u32,
    dst_x: u32,
    dst_y: u32,
    mode: CompositionMode,
) {
    for row in 0..height as usize {
        let dst_off = ((dst_y as usize + row) * canvas_width as usize + dst_x as usize) * 4;
        let src_off = ((src_y as usize + row) * canvas_width as usize + src_x as usize) * 4;
        let len = width as usize * 4;
        // 源与目标可能在同一块缓冲上（同帧自合成），此时不能同时可变/不可变借用。
        if std::ptr::eq(dst.as_ptr(), src.as_ptr()) {
            // 矩形已由调用方校验不相交，逐字节处理即可。
            let src_pixels: Vec<u8> = src[src_off..src_off + len].to_vec();
            compose_row(&mut dst[dst_off..dst_off + len], &src_pixels, mode);
        } else {
            compose_row(
                &mut dst[dst_off..dst_off + len],
                &src[src_off..src_off + len],
                mode,
            );
        }
    }
}

/// 合成一行像素。
fn compose_row(dst: &mut [u8], src: &[u8], mode: CompositionMode) {
    match mode {
        CompositionMode::Overwrite => dst.copy_from_slice(src),
        CompositionMode::AlphaBlend => {
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                src_over(d, s);
            }
        }
    }
}

/// 非预乘的 src-over：`dst = src + dst * (1 - src.a)`。
///
/// 像素是**非预乘**存储（与 `DecodedImage` 保持一致），因此混合时需要先把
/// 各通道按 alpha 加权再除回结果 alpha。
fn src_over(dst: &mut [u8], src: &[u8]) {
    let sa = src[3] as u32;
    if sa == 255 {
        dst.copy_from_slice(src);
        return;
    }
    if sa == 0 {
        return;
    }
    let da = dst[3] as u32;
    let inv = 255 - sa;
    // 结果 alpha（四舍五入）。
    let out_a = sa + (da * inv + 127) / 255;
    if out_a == 0 {
        dst.copy_from_slice(&[0, 0, 0, 0]);
        return;
    }
    for c in 0..3 {
        // 分母是 out_a：先把 src 与 dst 都按各自 alpha 加权求和，再除回。
        let num = src[c] as u32 * sa * 255 + dst[c] as u32 * da * inv;
        dst[c] = ((num + (out_a * 255) / 2) / (out_a * 255)) as u8;
    }
    dst[3] = out_a as u8;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(color: u8, gap_ms: u32) -> Frame {
        Frame {
            data: vec![color, color, color, 255],
            gap_ms,
        }
    }

    fn anim_with(gaps: &[u32]) -> Animation {
        let mut a = Animation {
            root_gap_ms: gaps[0],
            ..Default::default()
        };
        for (i, gap) in gaps[1..].iter().enumerate() {
            a.frames.push(frame(i as u8 + 1, *gap));
        }
        a
    }

    #[test]
    fn frame_count_includes_root() {
        let a = anim_with(&[0, 40, 40]);
        assert_eq!(a.frame_count(), 3, "根帧 + 2 个存储帧");
    }

    #[test]
    fn gap_helpers_round_trip() {
        let mut a = anim_with(&[0, 100]);
        assert_eq!(a.gap_at(0), 0);
        assert_eq!(a.gap_at(1), 100);
        assert_eq!(a.duration_ms(), 100);

        a.set_gap_at(0, 40);
        a.set_gap_at(1, 60);
        assert_eq!(a.root_gap_ms, 40);
        assert_eq!(a.duration_ms(), 100);
    }

    #[test]
    fn frame_number_two_is_first_stored_frame() {
        let a = anim_with(&[0, 40, 40]);
        assert!(a.frame(1).is_none(), "帧 1 是图像自身数据，不在 frames 里");
        assert_eq!(a.frame(2).unwrap().gap_ms, 40);
        assert!(a.frame(4).is_none(), "帧编号超出范围");
    }

    #[test]
    fn frame_bytes_sums_stored_frames() {
        let a = anim_with(&[0, 40, 40]);
        assert_eq!(a.frame_bytes(), 8, "两个 4 字节帧");
    }

    #[test]
    fn stopped_animation_never_ticks() {
        let mut a = anim_with(&[40, 40]);
        assert_eq!(a.tick(0), (None, false));
    }

    #[test]
    fn zero_duration_never_ticks() {
        let mut a = anim_with(&[0, 0]);
        a.state = State::Running;
        assert_eq!(a.tick(0), (None, false), "全是无间隔帧 → 无可显示帧");
    }

    #[test]
    fn running_advances_after_gap_elapses() {
        let mut a = anim_with(&[40, 10]);
        a.state = State::Running;

        // 首个 tick 只锚定当前帧，不推进。
        let (delay, changed) = a.tick(0);
        assert_eq!(delay, Some(40), "应安排根帧间隔后的唤醒");
        assert!(!changed);
        assert_eq!(a.current_index, 0);

        // 间隔未走完：仍不推进。
        let (delay, changed) = a.tick(39);
        assert_eq!(delay, Some(1));
        assert!(!changed);

        // 间隔走完：推进到帧 1（协议帧 2），并安排它的间隔。
        let (delay, changed) = a.tick(40);
        assert!(changed, "换帧应标记内容变化");
        assert_eq!(a.current_index, 1);
        assert_eq!(delay, Some(10));
    }

    #[test]
    fn gapless_frames_are_skipped() {
        // 根帧 40ms，帧 2 无间隔（跳过），帧 3 有间隔。
        let mut a = anim_with(&[40, 0, 20]);
        a.state = State::Running;
        a.tick(0);
        let (_, changed) = a.tick(40);
        assert!(changed);
        assert_eq!(a.current_index, 2, "应直接跳到帧 3，跳过无间隔的帧 2");
    }

    #[test]
    fn loading_state_parks_on_last_frame_instead_of_wrapping() {
        let mut a = anim_with(&[10, 10]);
        a.state = State::Loading;
        a.tick(0);
        a.tick(10); // → 帧 1
        assert_eq!(a.current_index, 1);

        // 再走完间隔会绕回根帧，但 loading 应拒绝。
        let (delay, changed) = a.tick(20);
        assert!(!changed, "loading 状态不应绕回");
        assert_eq!(a.current_index, 1, "应停在最后一帧等新帧");
        assert_eq!(delay, None, "停住的动画不安排下一次 tick");
    }

    #[test]
    fn finite_loops_stop_after_budget() {
        // 两帧，v=2 → max_loops = 1。
        let mut a = anim_with(&[10, 10]);
        a.state = State::Running;
        a.max_loops = 1;
        a.tick(0);
        a.tick(10); // → 帧 1（current_index=1）
        assert_eq!(a.current_loop, 0);
        assert_eq!(a.current_index, 1);

        // 走完第二帧的间隔会绕回，轮数 +1 并耗尽额度 → 停住。
        let (delay, _) = a.tick(20);
        assert_eq!(a.current_loop, 1);
        assert_eq!(a.current_index, 1, "额度用尽应停在最后一帧");
        assert_eq!(delay, None);
    }

    #[test]
    fn tick_reanchors_when_clock_goes_backwards() {
        let mut a = anim_with(&[40, 40]);
        a.state = State::Running;
        a.tick(1000);
        // 时钟倒退：应重新锚定而不是傻等旧时间戳。
        let (delay, _) = a.tick(5);
        assert_eq!(delay, Some(40));
    }

    #[test]
    fn tick_advances_at_most_one_frame() {
        let mut a = anim_with(&[10, 10, 10]);
        a.state = State::Running;
        a.tick(0);
        // 时间一下跳很远，依然只推进一帧。
        a.tick(10_000);
        assert_eq!(a.current_index, 1, "不做追帧，每次 tick 至多一帧");
    }

    #[test]
    fn is_animating_requires_running_state_and_frames() {
        let mut a = anim_with(&[40, 40]);
        assert!(!a.is_animating(), "stopped 不算运行中");

        a.state = State::Running;
        assert!(a.is_animating());

        let bare = Animation {
            state: State::Running,
            root_gap_ms: 40,
            ..Default::default()
        };
        assert!(!bare.is_animating(), "只有根帧时没有可推进的目标");
    }

    #[test]
    fn background_bits_decode_to_rgba() {
        // 0xff0000ff → r=0xff（最低字节）、a=0xff（最高字节）。
        let bg = Background::from_bits(0xff0000ff);
        assert_eq!((bg.r, bg.g, bg.b, bg.a), (255, 0, 0, 255));
        assert!(!bg.is_zero());
        assert!(Background::from_bits(0).is_zero());
    }

    #[test]
    fn fill_background_paints_every_pixel() {
        let mut buf = vec![9u8; 8];
        fill_background(
            &mut buf,
            Background {
                r: 1,
                g: 2,
                b: 3,
                a: 4,
            },
        );
        assert_eq!(buf, vec![1, 2, 3, 4, 1, 2, 3, 4]);

        fill_background(&mut buf, Background::default());
        assert_eq!(buf, vec![0u8; 8]);
    }

    #[test]
    fn compose_rect_clips_at_canvas_edge() {
        // 2×2 画布，把 2×1 的矩形放在 (1,1)：只有第一个像素装得下。
        let mut dst = vec![0u8; 16];
        let src = vec![1, 2, 3, 4, 5, 6, 7, 8];
        compose_rect(&mut dst, 2, 2, &src, 2, 1, 1, 1, CompositionMode::Overwrite);

        let mut expect = vec![0u8; 16];
        expect[12..16].copy_from_slice(&[1, 2, 3, 4]);
        assert_eq!(dst, expect);
    }

    #[test]
    fn compose_rect_outside_canvas_is_noop() {
        let mut dst = vec![0u8; 16];
        compose_rect(
            &mut dst,
            2,
            2,
            &[1, 2, 3, 4],
            1,
            1,
            5,
            5,
            CompositionMode::Overwrite,
        );
        assert_eq!(dst, vec![0u8; 16]);
    }

    #[test]
    fn alpha_blend_matches_src_over() {
        // 不透明红打底，覆盖 50% 透明蓝：结果应是各半的紫。
        let mut dst = vec![255, 0, 0, 255];
        compose_rect(
            &mut dst,
            1,
            1,
            &[0, 0, 255, 128],
            1,
            1,
            0,
            0,
            CompositionMode::AlphaBlend,
        );
        // sa=128, inv=127, out_a = 128 + (255*127+127)/255 = 255
        assert_eq!(dst[3], 255, "结果应不透明");
        assert!(
            (dst[0] as i32 - 127).abs() <= 1,
            "红分量约一半，实际 {}",
            dst[0]
        );
        assert!(
            (dst[2] as i32 - 128).abs() <= 1,
            "蓝分量约一半，实际 {}",
            dst[2]
        );
    }

    #[test]
    fn alpha_blend_with_opaque_source_is_a_copy() {
        let mut dst = vec![1, 2, 3, 4];
        compose_rect(
            &mut dst,
            1,
            1,
            &[9, 8, 7, 255],
            1,
            1,
            0,
            0,
            CompositionMode::AlphaBlend,
        );
        assert_eq!(dst, vec![9, 8, 7, 255]);
    }

    #[test]
    fn alpha_blend_with_transparent_source_keeps_destination() {
        let mut dst = vec![1, 2, 3, 4];
        compose_rect(
            &mut dst,
            1,
            1,
            &[9, 8, 7, 0],
            1,
            1,
            0,
            0,
            CompositionMode::AlphaBlend,
        );
        assert_eq!(dst, vec![1, 2, 3, 4]);
    }

    #[test]
    fn compose_canvas_rect_moves_a_rect_within_a_canvas() {
        // 4×1 画布，把 (0,0)-(1,0) 的两像素拷到 (2,0)。
        let mut dst: Vec<u8> = vec![1, 0, 0, 255, 2, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0];
        let src = dst.clone();
        compose_canvas_rect(
            &mut dst,
            &src,
            4,
            2,
            1,
            0,
            0,
            2,
            0,
            CompositionMode::Overwrite,
        );
        assert_eq!(&dst[8..12], &[1, 0, 0, 255]);
        assert_eq!(&dst[12..16], &[2, 0, 0, 255]);
    }

    #[test]
    fn compose_canvas_rect_reads_arbitrary_source_offset() {
        // 4×1 画布，从 (2,0) 读两像素写到 (0,0)。
        let mut dst: Vec<u8> = vec![1, 0, 0, 255, 2, 0, 0, 255, 0, 0, 0, 0, 0, 0, 0, 0];
        let src = dst.clone();
        compose_canvas_rect(
            &mut dst,
            &src,
            4,
            2,
            1,
            2,
            0,
            0,
            0,
            CompositionMode::Overwrite,
        );
        assert_eq!(&dst[0..4], &[0, 0, 0, 0], "源像素 (2,0) 原本是透明");
        assert_eq!(&dst[4..8], &[0, 0, 0, 0]);
    }
}
