//! 图像与放置的存储：上限控制、淘汰策略、删除语义。

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use super::animation::Animation;
use super::image::{DecodedImage, ImageId};

/// 取图像存储锁；锁被投毒时恢复内部数据而不是 panic。
///
/// 读线程在持锁的 decode/execute 路径上 panic 一次就会毒化这把锁；旧代码
/// 之后的每个 `expect` 都会让 UI 线程继续 panic，整个终端窗口不可用。
/// 图像缓存丢一帧最多是画面短暂缺失，远好过进程崩溃。
pub fn lock_unpoisoned<T>(lock: &Mutex<T>) -> MutexGuard<'_, T> {
    lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 放置标识：`(图像 id, 放置 id)`。
///
/// 同一图像可以有多个放置（不同位置/尺寸），`p=` 区分它们。
pub type PlacementKey = (ImageId, u32);

/// z 层级分界：小于该值的放置画在文字**下方**。
///
/// kitty 语义是 `z < 0` 在文字下、`z >= 0` 在文字上（`z=0` 也在文字上），
/// 这里用常量把该判断集中到一处，避免渲染层重复硬编码。
pub const Z_BELOW_TEXT_LIMIT: i32 = 0;

/// 单张图像允许的放置上限。
///
/// 客户端可以用不同 `p=` 对同一图像反复 `a=p` 而不删除，旧实现会让
/// `HashMap` 无界增长（每个 placement 还带尺寸/偏移，几百字节起）。
/// 4096 已远超正常用法；超出时淘汰一个旧 placement，图像数据本身不动。
const MAX_PLACEMENTS_PER_IMAGE: usize = 4096;

/// 放置的位置类型。
///
/// 虚拟放置（`U=1`）本身**不参与绘制**：它只是一份"这块图像要怎么铺进网格"
/// 的尺寸说明（占几行几列），实际落位由网格里的 U+10EEEE 占位符决定。这样
/// 图像能跟着文字滚动/换行，而 `U=0` 的普通放置锚定在固定行上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlacementLocation {
    /// `U=0`：锚定在创建时的光标位置。
    #[default]
    Anchor,
    /// `U=1`：虚拟放置，由 Unicode 占位符决定实际位置。
    Virtual,
}

/// 一个放置占据的网格矩形（闭区间，两端都含）。
///
/// 对应 ghostty `graphics_image.zig:862` 的 `Rect`：图像无法只占半个单元格，
/// 所以边界向上取整到整格。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridRect {
    pub top: i32,
    pub bottom: i32,
    pub left: u32,
    pub right: u32,
}

impl GridRect {
    /// 指定单元格是否落在这个矩形内。
    ///
    /// 对齐 ghostty `graphics_image.zig:869` 的 `Rect.contains`：列独立比较
    /// （`top_left.x <= x <= bottom_right.x`），行也独立比较——两者都是闭区间，
    /// 不是「锚点相等」。这正是 `d=c`/`d=p`/`d=q` 的语义：删除**与目标单元格
    /// 相交**的放置，而不是恰好在目标格起点的放置。
    pub fn contains(&self, row: i32, col: u32) -> bool {
        col >= self.left && col <= self.right && row >= self.top && row <= self.bottom
    }

    /// 本矩形是否跨越列 `col`（`d=x` 语义）。
    pub fn spans_column(&self, col: u32) -> bool {
        col >= self.left && col <= self.right
    }

    /// 本矩形是否跨越行 `row`（`d=y` 语义）。
    pub fn spans_row(&self, row: i32) -> bool {
        row >= self.top && row <= self.bottom
    }
}

/// 单元格几何：推导未指定尺寸的放置需要它。
///
/// 对应 ghostty 从 `terminal.Terminal` 取的 `t.width_px / t.cols` 与
/// `t.height_px / t.rows`（`graphics_storage.zig:1970` 的 `gridSize`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellGeometry {
    pub cell_width: u32,
    pub cell_height: u32,
}

/// 当前可见的网格行窗口（闭区间，含两端）。
///
/// 行号用 alacritty 的 grid 绝对坐标：屏幕顶行是 `top`，向上滚出屏幕进入
/// scrollback 的行号为负。`d=a`/`d=A` 只删「屏幕上可见」的放置，需要它判定
/// 一个放置的锚点或底边是否仍落在窗口内——对应 ghostty
/// `graphics_storage.zig:1434` 用 `pointFromPin(.active, ...)` 判断某个 pin 是否
/// 还在活动区。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisibleRows {
    /// 可见的最上一行（含）。
    pub top: i32,
    /// 可见的最下一行（含）。
    pub bottom: i32,
}

impl VisibleRows {
    /// 由显示偏移与视口行数构造。`display_offset` 是向上滚出屏幕的行数，
    /// 所以屏幕顶行是 `-display_offset`。
    pub fn new(display_offset: usize, viewport_rows: usize) -> Self {
        let top = -(display_offset as i32);
        // 视口为 0 行时窗口为空：bottom < top，任何行都不在窗口内。
        let bottom = top.saturating_add(viewport_rows as i32).saturating_sub(1);
        Self { top, bottom }
    }

    /// 空窗口不包含任何行。
    pub fn is_empty(&self) -> bool {
        self.bottom < self.top
    }

    /// 某行是否可见。
    pub fn contains(&self, row: i32) -> bool {
        !self.is_empty() && row >= self.top && row <= self.bottom
    }
}

/// 一个图像放置：锚定到网格的哪一行/列、占多少行列、层级。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Placement {
    /// 锚定行（终端网格的绝对行号，随滚动变化）。
    pub row: i32,
    /// 锚定列。
    pub col: u32,
    /// 占用的列数（`c=`）；为 `None` 表示按图像原始像素推导。
    pub columns: Option<u32>,
    /// 占用的行数（`r=`）；为 `None` 表示按宽高比推导。
    pub rows: Option<u32>,
    /// 单元格内的像素偏移（`X=`/`Y=`），取值范围为单个单元格内。
    pub x_offset: u32,
    pub y_offset: u32,
    /// z 层级。`< 0` 画在文字下方，`>= 0` 画在文字上方。
    pub z: i32,
    /// 位置类型（`U=`）。虚拟放置不直接绘制。
    pub location: PlacementLocation,
}

impl Placement {
    /// 是否画在文字下方。
    pub fn is_below_text(&self) -> bool {
        self.z < Z_BELOW_TEXT_LIMIT
    }

    /// 是否是虚拟放置（由占位符驱动，自身不绘制）。
    pub fn is_virtual(&self) -> bool {
        self.location == PlacementLocation::Virtual
    }

    /// 本放置占据的网格行列数。
    ///
    /// 逐行对齐 ghostty `graphics_storage.zig:1970` 的 `gridSize`：
    /// - `c=`/`r=` **都**给出时直接用（规范规定此时按给定格数铺满，图像被拉伸）；
    /// - 否则按 `图像像素 + 单元格内偏移` 除以单元格尺寸、**向上取整**到整格
    ///   （ghostty 的 `pixelSize +| cellOffset`；注释点明取整方向：图像占不满
    ///   一格也要占一格）。偏移同样要对齐——一个小图配一个接近整格的 `X=`
    ///   偏移时会多占一列，漏掉这个加法就会少删一列。
    ///
    /// 只给 `c=` 或只给 `r=` 的情况 ghostty 并不单独处理——`gridSize` 只在
    /// 「两者都 > 0」时走显式分支，否则一律回落到像素推导。deepterm 的渲染层
    /// （`render.rs:placement_rect`）会按宽高比补出另一边，那是**绘制**的尺寸；
    /// 删除判定必须与 ghostty 一致，所以这里不引入那个补算。
    pub fn grid_size(&self, image: &DecodedImage, geometry: CellGeometry) -> (u32, u32) {
        if let (Some(cols), Some(rows)) = (self.columns, self.rows)
            && cols > 0
            && rows > 0
        {
            return (cols, rows);
        }

        // 单元格尺寸为 0 时无法推导（ghostty 对除零返回 0 并注释为安全）。
        if geometry.cell_width == 0 || geometry.cell_height == 0 {
            return (0, 0);
        }

        // `cellOffset`：偏移被夹在单元格内，先夹再相加。
        let offset_x = self.x_offset.min(geometry.cell_width - 1);
        let offset_y = self.y_offset.min(geometry.cell_height - 1);

        let cols = image
            .width
            .saturating_add(offset_x)
            .div_ceil(geometry.cell_width);
        let rows = image
            .height
            .saturating_add(offset_y)
            .div_ceil(geometry.cell_height);
        (cols, rows)
    }

    /// 本放置占据的网格矩形；虚拟放置返回 `None`。
    ///
    /// 对齐 ghostty `graphics_storage.zig:2080` 的 `Placement.rect`：
    /// - 虚拟放置没有自己的屏幕位置，几何查询一律不匹配（它们随父链移除）；
    /// - 行列数为 0 的放置不占任何矩形；
    /// - 右边界是 `min(col + cols - 1, 终端列数 - 1)`——减一是因为 `col`
    ///   本身已经占了一格（ghostty 在该处有同样的注释），并夹到终端宽度内。
    pub fn grid_rect(
        &self,
        image: &DecodedImage,
        geometry: CellGeometry,
        terminal_cols: u32,
    ) -> Option<GridRect> {
        if self.is_virtual() {
            return None;
        }

        let (cols, rows) = self.grid_size(image, geometry);
        if cols == 0 || rows == 0 {
            return None;
        }

        let right = self
            .col
            .saturating_add(cols - 1)
            .min(terminal_cols.saturating_sub(1));
        Some(GridRect {
            top: self.row,
            bottom: self.row.saturating_add_unsigned(rows - 1),
            left: self.col,
            right,
        })
    }

    /// 本放置是否在给定的可见行窗口内（`d=a`/`d=A` 的选择条件）。
    ///
    /// 对齐 ghostty `graphics_storage.zig:1414` 的 `deleteVisiblePlacements`：
    /// - 虚拟放置（以及以父链定位的 `relative`）永远不算可见，它们随父放置
    ///   一起移除；
    /// - 锚点落在可见窗口内即视为可见（图像顶部还在屏幕上）；
    /// - 锚点已滚出上方时，只要**底边**仍探进可见区（大图的上半部分滚出去
    ///   了、下半部分还露着）同样算可见——ghostty 用 `rect.bottom_right` 做
    ///   这第二次判定。
    ///
    /// 几何缺失（拿不到单元格尺寸）时无法算出底边，退化为只看锚点。
    pub fn visible_in(
        &self,
        image: &DecodedImage,
        geometry: CellGeometry,
        visible: VisibleRows,
    ) -> bool {
        if self.is_virtual() {
            return false;
        }
        if visible.contains(self.row) {
            return true;
        }
        // 锚点已在窗口上方（或下方）：只有当底边仍探进窗口才算可见。底边由
        // 网格行数决定，与列方向的裁剪无关，所以这里不走 `grid_rect`（它会
        // 夹列宽，且大步数会饱和到 i32::MAX 误判成可见）。
        if self.row > visible.bottom {
            return false;
        }
        let (_, rows) = self.grid_size(image, geometry);
        if rows == 0 {
            return false;
        }
        let bottom = self.row.saturating_add_unsigned(rows - 1);
        visible.contains(bottom)
    }
}

/// 存储中的一张图像及其放置。
#[derive(Debug)]
pub struct StoredImage {
    pub image: DecodedImage,
    /// 客户端通过 `I=` 指定的编号（用于 `d=n` 删除"最新一张"）。
    pub number: Option<u32>,
    /// 是否由 `a=t`（仅传输）创建，尚未被放置引用。
    pub transient: bool,
    /// 单调递增的世代号，用于淘汰时判定"最旧"。
    ///
    /// 同时是渲染层纹理缓存的失效键：动画换帧或原地编辑帧内容时，即使图像
    /// 本身没被重新插入，也要换一个新世代，否则缓存会一直复用旧像素。
    pub generation: u64,
    /// 该图像的所有放置。
    pub placements: HashMap<u32, Placement>,
    /// 动画状态；由第一条 `a=f` / `a=a` / `a=c` 命令惰性创建。
    pub animation: Option<Animation>,
}

impl StoredImage {
    /// 是否已被放置引用（淘汰优先级的重要依据）。
    pub fn is_placed(&self) -> bool {
        !self.placements.is_empty()
    }

    /// 图像字节数（含所有动画帧）。
    pub fn byte_len(&self) -> usize {
        self.image.byte_len() + self.animation.as_ref().map_or(0, |a| a.frame_bytes())
    }

    /// 取协议帧号 `number` 的像素数据。
    ///
    /// 帧 1（根帧）恒存在（只要图像数据完整），即使该图像没有动画状态；
    /// 帧 `2..N` 来自动画状态。
    pub fn frame_data(&self, number: u32) -> Option<&[u8]> {
        match number {
            0 => None,
            1 => Some(&self.image.rgba),
            _ => self
                .animation
                .as_ref()?
                .frame(number)
                .map(|f| f.data.as_slice()),
        }
    }

    /// 当前应显示的像素数据：有动画时是当前帧，否则是图像自身数据。
    pub fn render_data(&self) -> &[u8] {
        self.animation
            .as_ref()
            .and_then(|a| a.current_frame())
            .map(|f| f.data.as_slice())
            .unwrap_or(&self.image.rgba)
    }

    /// 当前应显示的帧的零基下标（无动画时为 0，即根帧）。
    pub fn current_frame_index(&self) -> u32 {
        self.animation.as_ref().map_or(0, |a| a.current_index)
    }
}

/// 淘汰优先级：数值越小越先被淘汰。
///
/// 顺序对齐 ghostty `graphics_storage.zig::evictImageExcept`：
/// 未被放置的优先淘汰，临时（transient）优先于非临时。
pub fn eviction_priority(image: &StoredImage) -> u8 {
    match (image.transient, image.is_placed()) {
        (true, false) => 0,
        (false, false) => 1,
        (true, true) => 2,
        (false, true) => 3,
    }
}

/// 删掉 `frames` 中下标 `removed` 的帧后，把 `current_index` 修正到语义
/// 最接近的存活帧上。
///
/// 规则与 ghostty `deleteAnimationFrame` 一致：帧数变少后若当前下标越界，
/// 就夹到最后一帧；删掉的正是当前帧则原地停住（`frame_shown_at_ms` 清空，
/// 让计时重新开始）；删的是当前帧之前的帧则左移一位。
fn fix_current_index(anim: &mut Animation, removed: usize) {
    let remaining = anim.frames.len() as u32;
    if anim.current_index > remaining {
        anim.current_index = remaining;
        anim.frame_shown_at_ms = None;
    } else if removed as u32 == anim.current_index {
        anim.frame_shown_at_ms = None;
    } else if (removed as u32) < anim.current_index {
        anim.current_index -= 1;
    }
}

/// 图像存储：按 id 索引，带字节上限与优先级淘汰。
#[derive(Debug)]
pub struct Storage {
    images: HashMap<ImageId, StoredImage>,
    total_bytes: usize,
    limit_bytes: usize,
    next_generation: u64,
    /// 隐式 id 的分配游标（从高到低，避开客户端显式 id 的常用区间）。
    next_implicit_id: u32,
}

impl Storage {
    /// 新建存储。`limit_bytes` 为 0 表示禁用整个协议。
    pub fn new(limit_bytes: usize) -> Self {
        Self {
            images: HashMap::new(),
            total_bytes: 0,
            limit_bytes,
            next_generation: 1,
            next_implicit_id: u32::MAX,
        }
    }

    /// 协议是否启用。
    pub fn enabled(&self) -> bool {
        self.limit_bytes > 0
    }

    /// 当前占用字节数。
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }

    /// 图像数量（测试用）。
    pub fn len(&self) -> usize {
        self.images.len()
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// 分配一个隐式图像 id（客户端未给 `i=` 时）。
    pub fn allocate_implicit_id(&mut self) -> ImageId {
        // 从高往低找空位，避免与客户端常用的低位 id 冲突。
        loop {
            let id = self.next_implicit_id;
            self.next_implicit_id = self.next_implicit_id.wrapping_sub(1);
            if !self.images.contains_key(&id) {
                return id;
            }
        }
    }

    /// 插入或替换一张图像。
    ///
    /// 替换同一 id 时，旧图像及其全部放置被丢弃（对齐 kitty：重新传输同一
    /// id 视为替换）。
    pub fn insert(
        &mut self,
        id: ImageId,
        image: DecodedImage,
        number: Option<u32>,
        transient: bool,
    ) {
        // 先移除同 id 的旧数据，保证字节计数正确。
        self.remove(id);

        let generation = self.alloc_generation();
        self.total_bytes += image.byte_len();

        self.images.insert(
            id,
            StoredImage {
                image,
                number,
                transient,
                generation,
                placements: HashMap::new(),
                // 重新传输同一 id 会重置动画（协议规定），这里不搬运旧动画。
                animation: None,
            },
        );

        self.evict_until_within_limit(Some(id));
    }

    /// 分配一个新世代号。
    fn alloc_generation(&mut self) -> u64 {
        let generation = self.next_generation;
        self.next_generation += 1;
        generation
    }

    /// 标记某个图像**显示内容**变了，但图像并未被重新插入。
    ///
    /// 用于动画换帧、原地编辑帧像素等场景：换发新世代号让渲染层的纹理缓存
    /// 失效，从而把新像素取进来。与重新 `insert` 不同，这里不动字节计数。
    pub fn mark_content_changed(&mut self, id: ImageId) {
        let generation = self.alloc_generation();
        if let Some(stored) = self.images.get_mut(&id) {
            stored.generation = generation;
        }
    }

    /// 在既有共享存储上追加一段帧数据预留。
    ///
    /// 供动画新建帧使用：先按 `bytes` 预留空间（必要时淘汰**其他**图像），
    /// 成功后再真正分配。返回 `false` 表示腾不出空间。
    ///
    /// `exclude` 是本次预留的受益图像，永不被淘汰（否则刚建的图像会把自己
    /// 挤掉）。
    ///
    /// **失败时不改动任何状态**：会先算清可淘汰的字节总量再动手，避免
    /// "淘汰了一批图像却仍然腾不出空间"这种把存储搅坏一半的结果。
    pub fn reserve_animation_bytes(&mut self, exclude: ImageId, bytes: usize) -> bool {
        if self.limit_bytes == 0 {
            return false;
        }
        if bytes > self.limit_bytes {
            return false;
        }
        if self.total_bytes + bytes <= self.limit_bytes {
            self.total_bytes += bytes;
            return true;
        }

        let required = self.total_bytes + bytes - self.limit_bytes;

        // 先把"可淘汰的总字节"算出来，不够就直接失败、不碰任何图像。
        let evictable: usize = self
            .images
            .iter()
            .filter(|(id, _)| **id != exclude)
            .map(|(_, img)| img.byte_len())
            .sum();
        if evictable < required {
            log::debug!(
                "kitty 动画帧预留 {} 字节失败：可淘汰 {} < 需要 {}",
                bytes,
                evictable,
                required
            );
            return false;
        }

        let mut freed = 0usize;
        while freed < required {
            let before = self.total_bytes;
            if !self.evict_one(Some(exclude)) {
                // 上面的总量检查保证走不到这里；兜底返回失败。
                return false;
            }
            freed += before - self.total_bytes;
        }

        self.total_bytes += bytes;
        true
    }

    /// 释放一段帧数据预留（或归还已删除帧数据的字节）。
    pub fn release_animation_bytes(&mut self, bytes: usize) {
        self.total_bytes = self.total_bytes.saturating_sub(bytes);
    }

    /// 推进所有正在播放的动画，返回下一次帧切换的延迟（毫秒）。
    ///
    /// `now_ms` 是调用方选定时钟上的单调时间戳，所有调用必须用同一时钟。
    /// 调用方是渲染层：每构建一帧 tick 一次，按返回值安排唤醒。
    ///
    /// 门槛与 ghostty `animationTick` 一致：
    /// - 没有动画状态、或状态是 stopped（帧只由客户端驱动）→ 跳过；
    /// - 只有根帧（没有可推进的目标）→ 跳过；
    /// - **未被放置**的图像不推进（简化的"可见"判断，避免从未显示的图像
    ///   永远唤醒渲染器）；
    /// - 总时长为 0（每帧都无间隔）→ 跳过；
    /// - 有限循环额度已用尽 → 跳过（播放已永久停在最后一帧）。
    pub fn animation_tick(&mut self, now_ms: u64) -> Option<u64> {
        let mut min_delay: Option<u64> = None;
        let mut changed_ids: Vec<ImageId> = Vec::new();

        for (id, stored) in self.images.iter_mut() {
            if !stored.is_placed() {
                continue;
            }
            let Some(anim) = stored.animation.as_mut() else {
                continue;
            };
            if !anim.is_animating() {
                continue;
            }
            if anim.max_loops > 0 && anim.current_loop >= anim.max_loops {
                continue;
            }

            let (delay, content_changed) = anim.tick(now_ms);
            if content_changed {
                changed_ids.push(*id);
            }
            if let Some(delay) = delay {
                min_delay = Some(min_delay.map_or(delay, |m: u64| m.min(delay)));
            }
        }

        // 换帧意味着显示像素变了，换发新世代让渲染层的纹理缓存失效。
        for id in changed_ids {
            self.mark_content_changed(id);
        }

        min_delay
    }

    /// 写入一个放置。
    pub fn place(&mut self, id: ImageId, placement_id: u32, placement: Placement) -> bool {
        let Some(stored) = self.images.get_mut(&id) else {
            return false;
        };
        // 一旦被放置就不再是"临时"的——影响淘汰优先级。
        stored.transient = false;
        if !stored.placements.contains_key(&placement_id)
            && stored.placements.len() >= MAX_PLACEMENTS_PER_IMAGE
            && let Some(oldest) = stored.placements.keys().next().copied()
        {
            stored.placements.remove(&oldest);
        }
        stored.placements.insert(placement_id, placement);
        true
    }

    /// 取图像。
    pub fn get(&self, id: ImageId) -> Option<&StoredImage> {
        self.images.get(&id)
    }

    /// 取图像（可变）。
    pub fn get_mut(&mut self, id: ImageId) -> Option<&mut StoredImage> {
        self.images.get_mut(&id)
    }

    /// 找占位符指向的放置。
    ///
    /// 语义对齐 ghostty `graphics_storage.zig::placeholderTarget`：
    /// - `placement_id != 0` 时**精确匹配**该放置，不要求它是虚拟放置：
    ///   占位符单元格只是"把某个放置画在这里"，指向普通放置是合法的
    ///   （ghostty 只有在 `U=1` 与 `P=` 同时出现时才报 EINVAL）；
    /// - `placement_id == 0` 时取该图像的**第一个**虚拟放置，让客户端可以
    ///   完全不在下划线色里编码放置 id；ghostty 用 `preferredOver` 保证
    ///   确定（external 优先、同 tag 下 id 小者优先），deepterm 的放置 id
    ///   全部由客户端指定，故"id 最小"即等价规则；
    /// - 找不到就返回 `None`，对应的占位符不绘制。
    ///
    /// 返回 `(placement_id, &Placement)`，调用方需要 `columns`/`rows` 来算
    /// 网格尺寸。
    pub fn placeholder_target(&self, id: ImageId, placement_id: u32) -> Option<(u32, &Placement)> {
        let stored = self.images.get(&id)?;

        if placement_id != 0 {
            let placement = stored.placements.get(&placement_id)?;
            return Some((placement_id, placement));
        }

        // 未指定放置 id：取第一个虚拟放置。用最小 id 保证结果确定
        // （HashMap 迭代顺序不稳定，取"任意一个"会让画面随机抖动）。
        stored
            .placements
            .iter()
            .filter(|(_, p)| p.is_virtual())
            .min_by_key(|(pid, _)| **pid)
            .map(|(pid, p)| (*pid, p))
    }

    /// 某张图像的全部虚拟放置 `(placement_id, &Placement)`，按 id 升序。
    pub fn virtual_placements(&self, id: ImageId) -> Vec<(u32, &Placement)> {
        let Some(stored) = self.images.get(&id) else {
            return Vec::new();
        };
        let mut out: Vec<(u32, &Placement)> = stored
            .placements
            .iter()
            .filter(|(_, p)| p.is_virtual())
            .map(|(pid, p)| (*pid, p))
            .collect();
        out.sort_by_key(|(pid, _)| *pid);
        out
    }

    /// 迭代全部图像。
    pub fn iter(&self) -> impl Iterator<Item = (&ImageId, &StoredImage)> {
        self.images.iter()
    }

    /// 移除一张图像及其全部放置。
    pub fn remove(&mut self, id: ImageId) -> Option<StoredImage> {
        let removed = self.images.remove(&id)?;
        self.total_bytes = self.total_bytes.saturating_sub(removed.byte_len());
        Some(removed)
    }

    /// 移除某个放置。图像本身保留。
    pub fn remove_placement(&mut self, id: ImageId, placement_id: u32) -> bool {
        self.images
            .get_mut(&id)
            .is_some_and(|stored| stored.placements.remove(&placement_id).is_some())
    }

    /// 删除**屏幕上可见**的放置（`d=a`/`d=A`）。返回是否发生了变化。
    ///
    /// 对齐 ghostty `graphics_storage.zig:1414` 的 `deleteVisiblePlacements` 与
    /// kitty 协议原文："delete all placements visible on screen"。滚出到
    /// scrollback 里的放置**不删**——客户端滚动回看时图像得还在。
    ///
    /// `free_data` 为真（大写 `d=A`）时，放置被删空且不再被引用的图像数据一并
    /// 释放（协议："provided that the image is not referenced elsewhere, such as
    /// in the scrollback buffer"）。
    pub fn delete_visible_placements(
        &mut self,
        geometry: CellGeometry,
        visible: VisibleRows,
        free_data: bool,
    ) -> bool {
        // 先收集待删项，避免在持有不可变借用时又要可变借用。
        let mut to_remove: Vec<(ImageId, u32)> = Vec::new();
        for (id, stored) in self.images.iter() {
            for (pid, p) in stored.placements.iter() {
                if p.visible_in(&stored.image, geometry, visible) {
                    to_remove.push((*id, *pid));
                }
            }
        }

        if to_remove.is_empty() {
            return false;
        }

        let mut emptied: Vec<ImageId> = Vec::new();
        for (id, pid) in to_remove {
            self.remove_placement(id, pid);
            if free_data
                && self
                    .images
                    .get(&id)
                    .is_some_and(|stored| stored.placements.is_empty())
                && !emptied.contains(&id)
            {
                emptied.push(id);
            }
        }

        for id in emptied {
            self.remove(id);
        }
        true
    }

    /// 清空全部图像。
    pub fn clear(&mut self) {
        self.images.clear();
        self.total_bytes = 0;
    }

    /// 找出拥有指定 `I=` 编号的最新图像（`d=n`）。
    pub fn newest_by_number(&self, number: u32) -> Option<ImageId> {
        self.images
            .iter()
            .filter(|(_, img)| img.number == Some(number))
            .max_by_key(|(_, img)| img.generation)
            .map(|(id, _)| *id)
    }

    /// 删除动画帧（`d=f` / `d=F`）。返回是否发生了变化。
    ///
    /// 帧号会被夹紧：0 选中根帧，超过末帧则选中最后一帧。删除根帧会把帧 2
    /// **提升为新的根帧**（它的字节预留不变，只释放旧根数据）。
    ///
    /// `delete` 为真（大写 `d=F`）时，若该图像根本没有动画帧，则连图像本体
    /// 一起删除——即使它还有放置，对齐 ghostty 的行为。
    pub fn delete_animation_frame(&mut self, id: ImageId, frame: u32, delete: bool) -> bool {
        let Some(stored) = self.images.get(&id) else {
            return false;
        };

        // 没有动画（或动画里一帧都没有）时只有大写形式才有意义。
        let has_frames = stored
            .animation
            .as_ref()
            .is_some_and(|a| !a.frames.is_empty());
        if !has_frames {
            if !delete {
                return false;
            }
            return self.remove(id).is_some();
        }

        // 夹紧帧号并取出动画：此时 anim 一定存在。先算出要释放的字节数，
        // 避免在借用 `stored` 期间又去改 `total_bytes`。
        let (removed_bytes, changed) = {
            let stored = self.images.get_mut(&id).expect("刚确认存在");
            let image_len = stored.image.rgba.len();
            let anim = stored
                .animation
                .as_mut()
                .expect("has_frames 已确认动画存在");

            let count = anim.frame_count();
            let number = if frame == 0 { 1 } else { frame.min(count) };

            if number == 1 {
                // 删除根帧：把帧 2 提升为新根帧，其字节预留保持不变。
                if anim.frames.is_empty() {
                    return false;
                }
                let promoted = anim.frames.remove(0);
                stored.image.rgba = promoted.data;
                anim.root_gap_ms = promoted.gap_ms;
                // 新根帧的尺寸与旧的相同（帧恒为整幅大小），故上面的赋值安全。
                (image_len, true)
            } else {
                let index = number - 2;
                let removed = anim.frames.remove(index as usize);
                let bytes = removed.data.len();
                fix_current_index(anim, index as usize);
                (bytes, true)
            }
        };

        if changed {
            self.release_animation_bytes(removed_bytes);
        }
        changed
    }

    /// 淘汰直到回到上限之内。
    ///
    /// `protect` 指定本次刚插入、不应被淘汰的 id。
    fn evict_until_within_limit(&mut self, protect: Option<ImageId>) {
        if self.limit_bytes == 0 {
            self.clear();
            return;
        }

        // 对齐 ghostty 的淘汰日志：常驻内存异常时，用
        // `RUST_LOG=deepterm::terminal::kitty=debug` 可直接看到淘汰频率与量级。
        // 若淘汰日志刷屏而总量不降，说明客户端在持续灌入新图（而非泄漏）。
        let over = self.total_bytes.saturating_sub(self.limit_bytes);
        if over > 0 {
            log::debug!(
                "kitty 图像存储超限 {} 字节（总量 {} / 上限 {}），开始淘汰",
                over,
                self.total_bytes,
                self.limit_bytes
            );
        }
        while self.total_bytes > self.limit_bytes {
            if !self.evict_one(protect) {
                // 只剩受保护的那张：它本身就超限，无法再淘汰。
                log::debug!(
                    "kitty 图像存储仍超限 {} 字节，但只剩受保护图像，停止淘汰",
                    self.total_bytes.saturating_sub(self.limit_bytes)
                );
                break;
            }
        }
    }

    /// 淘汰一个最该被淘汰的图像，返回是否真的淘汰了一个。
    fn evict_one(&mut self, protect: Option<ImageId>) -> bool {
        // 排序键：(淘汰优先级, generation) —— 优先级低者先淘汰，
        // 同优先级下最旧者先淘汰。
        let victim = self
            .images
            .iter()
            .filter(|(id, _)| Some(**id) != protect)
            .min_by_key(|(_, img)| (eviction_priority(img), img.generation))
            .map(|(id, _)| *id);

        match victim {
            Some(id) => {
                self.remove(id);
                true
            }
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(width: u32, height: u32) -> DecodedImage {
        DecodedImage {
            width,
            height,
            rgba: vec![0; (width * height * 4) as usize],
        }
    }

    fn geom() -> CellGeometry {
        CellGeometry {
            cell_width: 10,
            cell_height: 20,
        }
    }

    fn placement(row: i32, col: u32) -> Placement {
        Placement {
            row,
            col,
            ..Placement::default()
        }
    }

    // ===== 网格矩形（ghostty `gridSize` / `rect`）=====

    /// `c=`/`r=` 都给出时直接用（规范：按给定格数铺满，图像被拉伸）。
    #[test]
    fn grid_size_prefers_explicit_cols_and_rows() {
        let p = Placement {
            columns: Some(5),
            rows: Some(3),
            ..placement(2, 2)
        };
        assert_eq!(p.grid_size(&img(8, 8), geom()), (5, 3));
    }

    /// 只给一边时不走显式分支——ghostty 只在「两者都 > 0」时直接用，否则回落到
    /// 像素推导。这一点容易写错成「补出另一边」，但删除判定必须与 ghostty 一致。
    #[test]
    fn grid_size_falls_back_to_pixels_when_only_one_side_given() {
        // 20x40 像素 / 10x20 格 = 2x2 格；只有 c= 时也只按像素算。
        let p = Placement {
            columns: Some(9),
            rows: None,
            ..placement(0, 0)
        };
        assert_eq!(p.grid_size(&img(20, 40), geom()), (2, 2));
    }

    /// 像素推导必须**向上取整**：图像占不满一格也要占一格。
    #[test]
    fn grid_size_rounds_up_to_whole_cells() {
        // 25x45 像素 / 10x20 → 2.5 与 2.25，向上取整为 3x3。
        let p = placement(0, 0);
        assert_eq!(p.grid_size(&img(25, 45), geom()), (3, 3));

        // 3x5 像素不足一格，仍然占 1x1。
        assert_eq!(p.grid_size(&img(3, 5), geom()), (1, 1));
    }

    /// 单元格尺寸为 0 时无法推导（ghostty 对除零返回 0）。
    #[test]
    fn grid_size_is_zero_when_cell_size_unavailable() {
        let zero = CellGeometry {
            cell_width: 0,
            cell_height: 0,
        };
        assert_eq!(placement(0, 0).grid_size(&img(20, 40), zero), (0, 0));
    }

    /// `X=`/`Y=` 偏移计入像素尺寸（ghostty `pixelSize +| cellOffset`）。
    ///
    /// 10 像素宽的图配 9 像素偏移，合计 19 > 10，必须多占一列；忽略偏移就
    /// 会把它当成 1 列，删除时漏掉右边那一列。
    #[test]
    fn grid_size_includes_cell_offsets() {
        let p = Placement {
            x_offset: 9,
            y_offset: 9,
            ..placement(0, 0)
        };
        assert_eq!(p.grid_size(&img(10, 10), geom()), (2, 1));
    }

    /// 偏移夹在单元格内（ghostty `cellOffset` 的 `@min(offset, cell_size-1)`）。
    ///
    /// 50 像素偏移远超单元格尺寸，x 夹到 9（格宽 10）、y 夹到 19（格高 20），
    /// 与 10 像素图相加得 19 / 29 → 2 列 2 行。不夹就会算出 6 列 3 行。
    #[test]
    fn grid_size_clamps_cell_offsets_to_one_cell() {
        let p = Placement {
            x_offset: 50,
            y_offset: 50,
            ..placement(0, 0)
        };
        assert_eq!(p.grid_size(&img(10, 10), geom()), (2, 2));
    }

    /// 矩形的右/下边界是 `起点 + 格数 - 1`：起点本身已占一格。
    #[test]
    fn grid_rect_uses_inclusive_bounds() {
        let p = Placement {
            columns: Some(5),
            rows: Some(3),
            ..placement(2, 2)
        };
        let rect = p.grid_rect(&img(8, 8), geom(), 80).unwrap();
        assert_eq!(
            rect,
            GridRect {
                top: 2,
                bottom: 4,
                left: 2,
                right: 6
            }
        );
    }

    /// 右边界夹到终端列数之内（ghostty `rect()` 里的 `@min(.., t.cols - 1)`）。
    #[test]
    fn grid_rect_clamps_right_edge_to_terminal_width() {
        let p = Placement {
            columns: Some(10),
            rows: Some(1),
            ..placement(0, 75)
        };
        // 75 + 10 - 1 = 84 超出 80 列，应夹到 79。
        let rect = p.grid_rect(&img(8, 8), geom(), 80).unwrap();
        assert_eq!(rect.right, 79);
        assert!(rect.contains(0, 79));
        assert!(!rect.contains(0, 74), "左边界仍是 75");
    }

    /// 虚拟放置没有屏幕位置，几何查询一律不匹配。
    #[test]
    fn grid_rect_is_none_for_virtual_placement() {
        let p = Placement {
            columns: Some(5),
            rows: Some(3),
            location: PlacementLocation::Virtual,
            ..placement(2, 2)
        };
        assert!(p.grid_rect(&img(8, 8), geom(), 80).is_none());
    }

    /// 行列数为 0 的放置不占矩形。
    #[test]
    fn grid_rect_is_none_when_grid_size_is_zero() {
        let zero = CellGeometry {
            cell_width: 0,
            cell_height: 0,
        };
        assert!(placement(0, 0).grid_rect(&img(20, 40), zero, 80).is_none());
    }

    /// `contains` 是闭区间：四个角与内部都算“包含”。
    #[test]
    fn grid_rect_contains_is_inclusive_on_all_edges() {
        let rect = GridRect {
            top: 2,
            bottom: 4,
            left: 2,
            right: 6,
        };
        assert!(rect.contains(2, 2), "左上角");
        assert!(rect.contains(4, 6), "右下角");
        assert!(rect.contains(3, 4), "内部");
        assert!(rect.contains(2, 6), "右上角");
        assert!(rect.contains(4, 2), "左下角");

        assert!(!rect.contains(1, 4), "上方一行之外");
        assert!(!rect.contains(5, 4), "下方一行之外");
        assert!(!rect.contains(3, 1), "左侧一列之外");
        assert!(!rect.contains(3, 7), "右侧一列之外");
    }

    #[test]
    fn grid_rect_span_predicates_match_the_interval() {
        let rect = GridRect {
            top: 2,
            bottom: 4,
            left: 2,
            right: 6,
        };
        assert!(rect.spans_column(2) && rect.spans_column(6));
        assert!(!rect.spans_column(1) && !rect.spans_column(7));
        assert!(rect.spans_row(2) && rect.spans_row(4));
        assert!(!rect.spans_row(1) && !rect.spans_row(5));
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        assert_eq!(s.get(1).unwrap().image.width, 1);
        assert_eq!(s.total_bytes(), 4);
    }

    #[test]
    fn replacing_same_id_frees_old_bytes() {
        let mut s = Storage::new(1024);
        s.insert(1, img(2, 2), None, false);
        assert_eq!(s.total_bytes(), 16);
        s.insert(1, img(1, 1), None, false);
        assert_eq!(s.total_bytes(), 4, "替换同一 id 应释放旧图像字节");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn evicts_oldest_unplaced_first() {
        // 每张图 4 字节，上限 8 字节 → 只能留 2 张。
        let mut s = Storage::new(8);
        s.insert(1, img(1, 1), None, true); // transient, unplaced → 最先淘汰
        s.insert(2, img(1, 1), None, false);
        s.insert(3, img(1, 1), None, false);

        assert!(s.total_bytes() <= 8);
        assert!(s.get(1).is_none(), "最早插入的未放置临时图像应被淘汰");
        assert!(s.get(3).is_some(), "最新插入的图像必须保留");
    }

    #[test]
    fn placed_images_outlive_unplaced() {
        let mut s = Storage::new(8);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, Placement::default());
        s.insert(2, img(1, 1), None, false); // 未放置
        s.insert(3, img(1, 1), None, false); // 未放置 → 应被淘汰

        assert!(s.get(1).is_some(), "已放置的图像应比未放置的更晚淘汰");
    }

    #[test]
    fn eviction_priorities_match_ghostty_order() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, true);
        assert_eq!(eviction_priority(s.get(1).unwrap()), 0, "临时+未放置");

        s.insert(2, img(1, 1), None, false);
        assert_eq!(eviction_priority(s.get(2).unwrap()), 1, "非临时+未放置");

        s.place(2, 0, Placement::default());
        assert_eq!(eviction_priority(s.get(2).unwrap()), 3, "非临时+已放置");
    }

    #[test]
    fn zero_limit_disables_storage() {
        let mut s = Storage::new(0);
        assert!(!s.enabled());
        s.insert(1, img(1, 1), None, false);
        assert!(s.is_empty(), "上限为 0 时不应保留任何图像");
    }

    #[test]
    fn placing_clears_transient() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, true);
        assert!(s.get(1).unwrap().transient);
        s.place(1, 0, Placement::default());
        assert!(!s.get(1).unwrap().transient, "被放置后不再是临时图像");
    }

    #[test]
    fn newest_by_number_picks_highest_generation() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), Some(7), false);
        s.insert(2, img(1, 1), Some(7), false);
        assert_eq!(s.newest_by_number(7), Some(2));
    }

    #[test]
    fn implicit_ids_do_not_collide() {
        let mut s = Storage::new(1024);
        let a = s.allocate_implicit_id();
        s.insert(a, img(1, 1), None, false);
        let b = s.allocate_implicit_id();
        assert_ne!(a, b);
        assert_eq!(b, u32::MAX - 1);
    }

    #[test]
    fn placements_are_capped_per_image() {
        let mut s = Storage::new(8 * 1024 * 1024);
        s.insert(1, img(1, 1), None, false);
        let placement = Placement::default();
        for id in 0..(MAX_PLACEMENTS_PER_IMAGE as u32 + 10) {
            assert!(s.place(1, id, placement.clone()));
        }
        let guard = s.get(1).expect("图像还在");
        assert_eq!(guard.placements.len(), MAX_PLACEMENTS_PER_IMAGE);
    }

    #[test]
    fn placements_independent_per_image() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, Placement::default());
        s.place(1, 5, Placement::default());
        assert_eq!(s.get(1).unwrap().placements.len(), 2);

        s.remove_placement(1, 0);
        assert_eq!(s.get(1).unwrap().placements.len(), 1);
        assert!(!s.remove_placement(1, 0), "重复删除应返回 false");
    }

    #[test]
    fn z_below_text_semantics() {
        let below = Placement {
            z: -1,
            ..Default::default()
        };
        let at_zero = Placement::default();
        assert!(below.is_below_text());
        assert!(!at_zero.is_below_text(), "z=0 按 kitty 语义画在文字上方");
    }

    // ---- 动画相关 ----

    use crate::terminal::kitty::animation::Frame;

    fn add_animation(s: &mut Storage, id: ImageId, frame_bytes: usize, frames: usize) {
        let stored = s.get_mut(id).unwrap();
        let anim = stored.animation.get_or_insert_with(Animation::default);
        for _ in 0..frames {
            anim.frames.push(Frame {
                data: vec![0; frame_bytes],
                gap_ms: 40,
            });
        }
    }

    #[test]
    fn animation_frames_count_toward_byte_limit() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        assert_eq!(s.total_bytes(), 4);

        // 帧数据是"自有"分配的（不经 reserve），这里直接加并手动记账，
        // 模拟 exec 里 reserve 之后附加的过程。
        let frame_len = 4;
        assert!(s.reserve_animation_bytes(1, frame_len));
        add_animation(&mut s, 1, frame_len, 1);

        assert_eq!(s.total_bytes(), 8, "帧字节应计入总占用");
        assert_eq!(s.get(1).unwrap().byte_len(), 8);
    }

    #[test]
    fn frame_data_resolves_root_and_stored_frames() {
        let mut s = Storage::new(1024);
        s.insert(
            1,
            DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 2, 3, 4],
            },
            None,
            false,
        );
        add_animation(&mut s, 1, 4, 2);

        let stored = s.get(1).unwrap();
        assert_eq!(stored.frame_data(0), None, "帧号 0 非法");
        assert_eq!(
            stored.frame_data(1),
            Some(&[1u8, 2, 3, 4][..]),
            "帧 1 是图像数据"
        );
        assert!(stored.frame_data(2).is_some(), "帧 2 是第一个存储帧");
        assert!(stored.frame_data(3).is_some());
        assert!(stored.frame_data(4).is_none(), "越界帧号");
    }

    #[test]
    fn render_data_follows_current_frame() {
        let mut s = Storage::new(1024);
        s.insert(
            1,
            DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![1, 1, 1, 1],
            },
            None,
            false,
        );
        add_animation(&mut s, 1, 4, 1);
        assert_eq!(
            s.get(1).unwrap().render_data(),
            &[1, 1, 1, 1],
            "根帧时用图像数据"
        );

        s.get_mut(1)
            .unwrap()
            .animation
            .as_mut()
            .unwrap()
            .current_index = 1;
        assert_eq!(
            s.get(1).unwrap().render_data(),
            &[0, 0, 0, 0],
            "换帧后应用帧数据"
        );
    }

    #[test]
    fn reserve_animation_bytes_evicts_other_images() {
        // 上限 11 字节：两张 1×1（各 4 字节）= 8 字节，再加 4 字节会超限。
        let mut s = Storage::new(11);
        s.insert(1, img(1, 1), None, false);
        s.insert(2, img(1, 1), None, false);
        assert_eq!(s.total_bytes(), 8);

        // 给图像 1 预留 4 字节：需淘汰图像 2，且不能淘汰图像 1。
        assert!(s.reserve_animation_bytes(1, 4));
        assert!(s.get(1).is_some(), "受益图像不能被淘汰");
        assert!(s.get(2).is_none(), "应淘汰其他图像腾空间");
        assert_eq!(s.total_bytes(), 8, "淘汰 4 字节后又预留 4 字节");
    }

    #[test]
    fn reserve_animation_bytes_fails_without_touching_storage() {
        // 上限 8 字节：底图 4 + 另一张 4 已占满，且预留需要 4 字节。
        // 淘汰另一张后是 4 + 4 = 8，其实够用；换一个更苛刻的配置：
        let mut s = Storage::new(8);
        s.insert(1, img(1, 1), None, false);
        s.insert(2, img(1, 1), None, false);

        // 预留 8 字节：淘汰图像 2 后是 4 + 8 = 12 > 8，仍然不够。
        assert!(!s.reserve_animation_bytes(1, 8), "腾不出空间应返回 false");
        assert!(s.get(1).is_some(), "底图应保留");
        assert!(s.get(2).is_some(), "失败时不应牺牲其他图像");
        assert_eq!(s.total_bytes(), 8, "失败时字节计数不应变化");
    }

    #[test]
    fn reserve_animation_bytes_fails_when_nothing_to_evict() {
        let mut s = Storage::new(8);
        s.insert(1, img(1, 1), None, false);
        // 没有任何其他图像可淘汰。
        assert!(!s.reserve_animation_bytes(1, 8), "腾不出空间应返回 false");
        assert_eq!(s.total_bytes(), 4, "失败时不应改动字节计数");
    }

    #[test]
    fn reserve_larger_than_limit_fails_immediately() {
        let mut s = Storage::new(8);
        s.insert(1, img(1, 1), None, false);
        assert!(!s.reserve_animation_bytes(1, 100), "超过总上限直接失败");
    }

    #[test]
    fn release_animation_bytes_credits_back() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        assert!(s.reserve_animation_bytes(1, 100));
        assert_eq!(s.total_bytes(), 104);
        s.release_animation_bytes(100);
        assert_eq!(s.total_bytes(), 4);
    }

    #[test]
    fn mark_content_changed_bumps_generation_without_bytes() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        let before = s.get(1).unwrap().generation;
        let bytes = s.total_bytes();

        s.mark_content_changed(1);
        assert_ne!(s.get(1).unwrap().generation, before, "换帧应换新世代");
        assert_eq!(s.total_bytes(), bytes, "换世代不改字节计数");
    }

    #[test]
    fn delete_animation_frame_removes_one_frame() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        assert!(s.reserve_animation_bytes(1, 8));
        add_animation(&mut s, 1, 4, 2);
        assert_eq!(s.total_bytes(), 12);

        // 删协议帧 3（下标 1）。
        assert!(s.delete_animation_frame(1, 3, false));
        assert_eq!(
            s.get(1).unwrap().animation.as_ref().unwrap().frames.len(),
            1
        );
        assert_eq!(s.total_bytes(), 8, "应释放被删帧的字节");
    }

    #[test]
    fn deleting_root_frame_promotes_second_frame() {
        let mut s = Storage::new(1024);
        s.insert(
            1,
            DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![9, 9, 9, 9],
            },
            None,
            false,
        );
        assert!(s.reserve_animation_bytes(1, 4));
        add_animation(&mut s, 1, 4, 1);
        // 把存储帧的内容改成可识别的值。
        s.get_mut(1).unwrap().animation.as_mut().unwrap().frames[0].data = vec![7, 7, 7, 7];

        assert!(s.delete_animation_frame(1, 1, false));
        let stored = s.get(1).unwrap();
        assert_eq!(stored.image.rgba, vec![7, 7, 7, 7], "帧 2 应提升为新根帧");
        assert!(stored.animation.as_ref().unwrap().frames.is_empty());
        assert_eq!(s.total_bytes(), 4, "旧根数据应被释放");
    }

    #[test]
    fn delete_frame_clamps_out_of_range_number() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        assert!(s.reserve_animation_bytes(1, 12));
        add_animation(&mut s, 1, 4, 3);

        // 帧号远超末帧 → 夹到最后一帧。
        assert!(s.delete_animation_frame(1, 99, false));
        assert_eq!(
            s.get(1).unwrap().animation.as_ref().unwrap().frames.len(),
            2
        );
    }

    #[test]
    fn uppercase_animation_delete_removes_plain_image() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, Placement::default());

        // 没有动画帧时，d=F 连图像本体一起删（即使有放置）。
        assert!(s.delete_animation_frame(1, 1, true));
        assert!(s.get(1).is_none(), "d=F 应删除非动画图像");
    }

    #[test]
    fn lowercase_animation_delete_on_plain_image_is_noop() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        assert!(!s.delete_animation_frame(1, 1, false));
        assert!(s.get(1).is_some(), "d=f 不应删除非动画图像");
    }

    #[test]
    fn delete_missing_image_is_noop() {
        let mut s = Storage::new(1024);
        assert!(!s.delete_animation_frame(42, 1, true));
    }

    #[test]
    fn deleting_current_frame_reanchors_timer() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        assert!(s.reserve_animation_bytes(1, 8));
        add_animation(&mut s, 1, 4, 2);

        {
            let anim = s.get_mut(1).unwrap().animation.as_mut().unwrap();
            anim.current_index = 1;
            anim.frame_shown_at_ms = Some(123);
        }
        // 删协议帧 2（下标 0，在当前帧之前）→ 当前下标应左移。
        assert!(s.delete_animation_frame(1, 2, false));
        let anim = s.get(1).unwrap().animation.as_ref().unwrap();
        assert_eq!(anim.current_index, 0, "删当前帧之前的帧应左移当前下标");
    }

    /// 把图像置于"正在播放"状态：有存储帧、状态 running、放置过。
    ///
    /// 根帧间隔也设为 `gap_ms`，否则根帧默认是无间隔（gap 0），首个 tick 会
    /// 直接跳过它——那样测的就不是"计时等待"而是"跳帧"了。
    fn running_animation(s: &mut Storage, id: ImageId, gap_ms: u32) {
        let stored = s.get_mut(id).unwrap();
        let anim = stored.animation.get_or_insert_with(Animation::default);
        anim.root_gap_ms = gap_ms;
        for _ in 0..2 {
            anim.frames.push(Frame {
                data: vec![0; 4],
                gap_ms,
            });
        }
        anim.state = super::super::animation::State::Running;
    }

    #[test]
    fn animation_tick_advances_frame_and_reports_delay() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, Placement::default());
        running_animation(&mut s, 1, 40);

        // 第一次 tick 只是记录起始时刻，不发生换帧，返回 40ms。
        let generation_before = s.get(1).unwrap().generation;
        assert_eq!(s.animation_tick(1000), Some(40));
        assert_eq!(
            s.get(1).unwrap().current_frame_index(),
            0,
            "首个 tick 不换帧"
        );
        assert_eq!(
            s.get(1).unwrap().generation,
            generation_before,
            "没换帧就不该换世代"
        );

        // 到点后推进到下一帧，并 bump 世代让纹理缓存失效。
        assert_eq!(s.animation_tick(1040), Some(40));
        assert_eq!(s.get(1).unwrap().current_frame_index(), 1);
        assert_ne!(
            s.get(1).unwrap().generation,
            generation_before,
            "换帧必须换世代"
        );
    }

    #[test]
    fn unplaced_images_do_not_animate() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        running_animation(&mut s, 1, 40);

        assert_eq!(s.animation_tick(1000), None, "未放置的图像不推进");
        assert_eq!(s.get(1).unwrap().current_frame_index(), 0);
    }

    #[test]
    fn stopped_animation_is_not_advanced() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, Placement::default());
        running_animation(&mut s, 1, 40);
        s.get_mut(1).unwrap().animation.as_mut().unwrap().state =
            super::super::animation::State::Stopped;

        assert_eq!(s.animation_tick(5000), None);
    }

    #[test]
    fn exhausted_loop_budget_stops_ticking() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, Placement::default());
        running_animation(&mut s, 1, 40);
        {
            let anim = s.get_mut(1).unwrap().animation.as_mut().unwrap();
            anim.max_loops = 2;
            anim.current_loop = 2;
        }
        assert_eq!(s.animation_tick(5000), None, "循环额度用尽不再唤醒");
    }

    #[test]
    fn animation_tick_reports_minimum_delay_across_images() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.insert(2, img(1, 1), None, false);
        s.place(1, 0, Placement::default());
        s.place(2, 0, Placement::default());
        running_animation(&mut s, 1, 100);
        running_animation(&mut s, 2, 20);

        assert_eq!(s.animation_tick(1000), Some(20), "取最短延迟");
    }

    // ---- 虚拟放置与占位符目标解析 ----

    fn virtual_placement(columns: u32, rows: u32) -> Placement {
        Placement {
            columns: Some(columns),
            rows: Some(rows),
            location: PlacementLocation::Virtual,
            ..Default::default()
        }
    }

    fn anchor_placement() -> Placement {
        Placement::default()
    }

    #[test]
    fn virtual_placement_is_detected() {
        assert!(virtual_placement(4, 2).is_virtual());
        assert!(!anchor_placement().is_virtual());
    }

    #[test]
    fn placeholder_target_with_explicit_id_matches_exactly() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, anchor_placement());
        s.place(1, 7, virtual_placement(4, 2));

        // 显式指定且确实指向虚拟放置 → 命中。
        let (pid, p) = s.placeholder_target(1, 7).expect("虚拟放置应命中");
        assert_eq!(pid, 7);
        assert_eq!(p.columns, Some(4));

        // 显式指定一个不存在的放置 id → 不命中。
        assert!(
            s.placeholder_target(1, 5).is_none(),
            "不存在的放置 id 应返回 None"
        );
    }

    #[test]
    fn placeholder_target_accepts_a_non_virtual_placement() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 3, anchor_placement());

        // ghostty 的 `placement_id > 0` 分支只做精确查找，不要求 `.virtual`：
        // 占位符单元格可以把一个普通放置画到指定位置。
        let (pid, p) = s
            .placeholder_target(1, 3)
            .expect("显式指向普通放置也应命中");
        assert_eq!(pid, 3);
        assert!(!p.is_virtual());
    }

    #[test]
    fn placeholder_target_zero_falls_back_to_first_virtual() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 0, anchor_placement());
        s.place(1, 7, virtual_placement(4, 2));

        // 放置 id 0 表示"未指定"，语义是取第一个虚拟放置而非查放置 0。
        let (pid, _) = s.placeholder_target(1, 0).expect("未指定时应回退");
        assert_eq!(pid, 7);
    }

    #[test]
    fn placeholder_target_zero_picks_first_virtual() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 9, anchor_placement());
        s.place(1, 5, virtual_placement(3, 3));
        s.place(1, 2, virtual_placement(8, 1));

        let (pid, _) = s.placeholder_target(1, 0).expect("应取到某个虚拟放置");
        assert_eq!(pid, 2, "未指定放置 id 时取 id 最小的虚拟放置");
    }

    #[test]
    fn placeholder_target_zero_never_falls_back_to_a_non_virtual_placement() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 3, anchor_placement());

        // 只有普通放置时，零 id 回退必须落空——否则图像会既锚定在光标处、
        // 又被当成占位符目标铺开。显式指向它才是合法的精确匹配。
        assert!(s.placeholder_target(1, 0).is_none());
        assert_eq!(s.placeholder_target(1, 3).map(|(pid, _)| pid), Some(3));
    }

    #[test]
    fn placeholder_target_missing_image_is_none() {
        let s = Storage::new(1024);
        assert!(s.placeholder_target(42, 0).is_none());
    }

    #[test]
    fn virtual_placements_lists_only_virtual_sorted() {
        let mut s = Storage::new(1024);
        s.insert(1, img(1, 1), None, false);
        s.place(1, 4, anchor_placement());
        s.place(1, 9, virtual_placement(2, 2));
        s.place(1, 1, virtual_placement(5, 1));

        let ids: Vec<u32> = s.virtual_placements(1).iter().map(|(p, _)| *p).collect();
        assert_eq!(ids, vec![1, 9], "只列虚拟放置，且按 id 升序");
        assert!(s.virtual_placements(42).is_empty(), "不存在的图像返回空");
    }
}
