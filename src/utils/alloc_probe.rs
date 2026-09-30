//! 测试 / 基准专用的内存观测工具。
//!
//! 提供两类互相独立的测量手段：
//!
//! 1. [`Footprint`]（仅 macOS）：进程 `phys_footprint`，即 Xcode 内存
//!    gauge 口径的物理驻留内存。用于度量「一个终端实体 + 渲染缓存的
//!    实际 RSS 占用」，给 scrollback 行数等容量参数提供外部口径证据。
//! 2. [`AllocObserver`]：通过自定义 `GlobalAlloc` 装饰器统计单线程内的
//!    堆分配次数 / 字节。用于度量渲染管线的每帧分配抖动（分配次数应
//!    随优化趋近于 0）。
//!
//! 使用方式（均为 `#[cfg(test)]` / bench 专用，不进正式产物）：
//! ```ignore
//! let baseline = Footprint::capture();
//! // ... 构造 / 驱动被测对象 ...
//! println!("{}", baseline.delta_str(Footprint::capture()));
//!
//! AllocObserver::scope(|stats| { ...; });  // 统计闭包内的分配
//! ```

use std::alloc::{GlobalAlloc, Layout};

// ─────────────────────────────────────────────────────────────────────────────
// phys_footprint（macOS）
// ─────────────────────────────────────────────────────────────────────────────

/// 一次 `phys_footprint` 快照（字节）。
///
/// **这是进程级的量**：`phys_footprint` 覆盖整个进程，不区分分配来自哪个
/// 线程。因此基于它的测量（如 `terminal_scrollback_footprint_report`）
/// 只在「同一时刻没有其他测试在分配大块内存」的前提下才成立——GPUI 测试默认
/// 多线程跑，谁拿到这个锁，别人的分配就会污染差值。
#[cfg(target_os = "macos")]
#[derive(Clone, Copy, Debug)]
pub struct Footprint(pub u64);

#[cfg(target_os = "macos")]
mod ffi {
    use mach2::mach_types::task_name_t;
    use mach2::message::mach_msg_type_number_t;
    use mach2::traps::mach_task_self;

    /// `struct task_vm_info` 的字段布局（<mach/task_info.h>，截至 rev1）。
    /// rev0 部分（phys_footprint 之前）字段序列照抄 SDK 头文件；
    /// 尾部 rev2+ 字段不取，task_info 按 count 截断填充。
    #[repr(C)]
    #[derive(Default)]
    pub struct TaskVmInfo {
        _virtual_size: u64,
        _region_count: i32,
        _page_size: i32,
        _resident_size: u64,
        _resident_size_peak: u64,
        _device: u64,
        _device_peak: u64,
        _internal: u64,
        _internal_peak: u64,
        _external: u64,
        _external_peak: u64,
        _reusable: u64,
        _reusable_peak: u64,
        _purgeable_volatile_pmap: u64,
        _purgeable_volatile_resident: u64,
        _purgeable_volatile_virtual: u64,
        _compressed: u64,
        _compressed_peak: u64,
        _compressed_lifetime: u64,
        phys_footprint: u64,
    }

    unsafe extern "C" {
        fn task_info(
            target_task: task_name_t,
            flavor: u32,
            task_info_out: *mut TaskVmInfo,
            task_info_out_cnt: *mut mach_msg_type_number_t,
        ) -> i32;
    }

    const TASK_VM_INFO: u32 = 22;
    /// task_vm_info 的 flavor 计数以自然整数宽为单位（int/uint）。
    const TASK_VM_INFO_COUNT: mach_msg_type_number_t =
        (std::mem::size_of::<TaskVmInfo>() / std::mem::size_of::<u32>()) as mach_msg_type_number_t;

    /// 返回当前进程 `phys_footprint`（字节）。失败时返回 `None`。
    pub fn phys_footprint() -> Option<u64> {
        // SAFETY：task_info 是纯查询 API；传入了正确的结构指针与计数。
        // mach_task_self() 返回当前任务的 send right，生命周期为进程级。
        let mut info = TaskVmInfo::default();
        let kr = unsafe {
            task_info(
                mach_task_self(),
                TASK_VM_INFO,
                &mut info,
                &mut { TASK_VM_INFO_COUNT },
            )
        };
        if kr != 0 {
            return None;
        }
        Some(info.phys_footprint)
    }
}

#[cfg(target_os = "macos")]
impl Footprint {
    /// 捕获当前进程的 phys_footprint。
    pub fn capture() -> Self {
        Self(ffi::phys_footprint().unwrap_or(0))
    }

    pub fn delta_bytes(self, later: Footprint) -> u64 {
        later.0.saturating_sub(self.0)
    }

    /// 人类可读的差值（供测试输出与基准报告）。
    pub fn delta_str(self, later: Footprint) -> String {
        format_bytes(later.0.saturating_sub(self.0))
    }
}

/// 字节数的 MB 表示（1 位小数）。
pub fn format_bytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
}

// ─────────────────────────────────────────────────────────────────────────────
// 分配计数（所有平台）
// ─────────────────────────────────────────────────────────────────────────────

use std::cell::Cell;

thread_local! {
    /// 当前线程的分配统计。仅 [`AllocObserver::scope`] 运行期间被订阅。
    static ALLOC_STATS: Cell<Option<*mut AllocStats>> = const { Cell::new(None) };
}

/// 一个统计周期内的分配统计（单线程）。
#[derive(Default, Debug, Clone, Copy)]
pub struct AllocStats {
    pub allocations: u64,
    pub bytes: u64,
    /// 本周期内出现的最大单次分配（字节）。
    pub max_single: u64,
    /// 观测期间 [`AllocStats::bytes`] 的历史峰值（绝对值）。
    pub live_bytes_high_water: u64,
}

// SAFETY：跨线程读裸指针 —— 但该指针只在 [`AllocObserver::scope`] 闭包
// 运行期间有效，而闭包由调用线程同步执行；GlobalAlloc 的多线程请求
// 通过 thread_local 判定只统计本线程，其余线程拿到的永远是 None。
unsafe impl Send for AllocStats {}

thread_local! {
    static LIVE_BYTES: Cell<u64> = const { Cell::new(0) };
}

/// 装饰系统分配器的计数分配器。
///
/// 通过 `thread_local` 标志仅在订阅线程上统计：注册期间该线程的每次
/// alloc 计入 [`AllocStats`]。用作 `#[global_allocator]` 时开销为一次
/// TLS 读 + 分支；未订阅线程零额外成本。
pub struct CountingAllocator<A> {
    pub inner: A,
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for CountingAllocator<A> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY：GlobalAlloc::alloc 的契约由 inner（被装饰的分配器）保证；
        // unsafe_op_in_unsafe_fn 需要 edition 2024 显式块。
        let ptr = unsafe { self.inner.alloc(layout) };
        let size = layout.size() as u64;
        LIVE_BYTES.with(|live| live.set(live.get().wrapping_add(size)));
        ALLOC_STATS.with(|cell| {
            if let Some(stats) = cell.get() {
                // SAFETY：stats 指向 scope 栈上的统计结构，scope 闭包
                // 同步返回前其生命周期一直有效（见 ALLOC_STATS 的 SAFETY 注释）。
                let stats = unsafe { &mut *stats };
                stats.allocations += 1;
                stats.bytes += size;
                if size > stats.max_single {
                    stats.max_single = size;
                }
                LIVE_BYTES.with(|live| {
                    let live = live.get();
                    if live > stats.live_bytes_high_water {
                        stats.live_bytes_high_water = live;
                    }
                });
            }
        });
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE_BYTES.with(|live| live.set(live.get().wrapping_sub(layout.size() as u64)));
        // SAFETY：同 alloc，dealloc 契约由 inner 保证。
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // 先按 dealloc 语义扣减旧值，再按 alloc 语义累加新值。
        LIVE_BYTES.with(|live| live.set(live.get().wrapping_sub(layout.size() as u64)));
        // SAFETY：同 alloc，realloc 契约由 inner 保证。
        let new_ptr = unsafe { self.inner.realloc(ptr, layout, new_size) };
        let size = new_size as u64;
        LIVE_BYTES.with(|live| live.set(live.get().wrapping_add(size)));
        ALLOC_STATS.with(|cell| {
            if let Some(stats) = cell.get() {
                // SAFETY：同 alloc。
                let stats = unsafe { &mut *stats };
                stats.allocations += 1;
                stats.bytes += size;
                if size > stats.max_single {
                    stats.max_single = size;
                }
            }
        });
        new_ptr
    }
}

/// 在闭包运行期间统计**当前线程**的堆分配。
///
/// 返回闭包结果与 [`AllocStats`]。其它线程的分配不计入。
///
/// # 示例
/// ```ignore
/// let (value, stats) = AllocObserver::scope(|| build_big_structure());
/// assert_eq!(stats.allocations, 0); // 零分配构造
/// ```
pub struct AllocObserver;

impl AllocObserver {
    pub fn scope<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
        let mut stats = AllocStats::default();
        let stats_ptr: *mut AllocStats = &mut stats;
        ALLOC_STATS.with(|cell| cell.set(Some(stats_ptr)));
        // 观测从零开始：清掉本线程此前活动留下的水位基线。
        LIVE_BYTES.with(|live| live.set(0));
        let result = f();
        ALLOC_STATS.with(|cell| cell.set(None));
        (result, stats)
    }
}
