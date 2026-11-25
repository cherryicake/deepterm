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
        // SAFETY:task_info 是纯查询 API；传入了正确的结构指针与计数。
        // mach_task_self() 返回当前任务的 send right，生命周期为进程级。
        let mut info = TaskVmInfo::default();
        let kr = unsafe {
            task_info(mach_task_self(), TASK_VM_INFO, &mut info, &mut {
                TASK_VM_INFO_COUNT
            })
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
///
/// 全字段是 `u64`，本来就能自动 `Send`——早期版本里带裸指针时才需要
/// `unsafe impl`，现在那个断言已经多余（留着反而会在将来加字段时变成假保证）。
#[derive(Default, Debug, Clone, Copy)]
pub struct AllocStats {
    pub allocations: u64,
    pub bytes: u64,
    /// 本周期内出现的最大单次分配（字节）。
    pub max_single: u64,
    /// 观测期间 [`AllocStats::bytes`] 的历史峰值（绝对值）。
    pub live_bytes_high_water: u64,
}

thread_local! {
    // 有符号的 scope 相对值：闭包可以释放在进入 scope 前分配的对象，
    // 此时计数会暂时为负，不能用 u64 回绕成一个虚假的超高水位。
    static LIVE_BYTES: Cell<i128> = const { Cell::new(0) };
}

fn adjust_live_bytes(delta: i128) {
    LIVE_BYTES.with(|live| live.set(live.get() + delta));
}

fn update_live_high_water(stats: &mut AllocStats) {
    LIVE_BYTES.with(|live| {
        let current = match u64::try_from(live.get()) {
            Ok(value) => value,
            Err(_) if live.get() < 0 => 0,
            Err(_) => u64::MAX,
        };
        stats.live_bytes_high_water = stats.live_bytes_high_water.max(current);
    });
}

fn record_allocation(size: u64) {
    ALLOC_STATS.with(|cell| {
        if let Some(stats) = cell.get() {
            // SAFETY: stats 指向仍存活的 scope 栈上统计结构；RAII guard 在正常
            // 返回与 panic 展开时都会先摘除指针，再让该栈帧失效。
            let stats = unsafe { &mut *stats };
            stats.allocations += 1;
            stats.bytes += size;
            stats.max_single = stats.max_single.max(size);
            update_live_high_water(stats);
        }
    });
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
        // SAFETY:GlobalAlloc::alloc 的契约由 inner（被装饰的分配器）保证；
        // unsafe_op_in_unsafe_fn 需要 edition 2024 显式块。
        let ptr = unsafe { self.inner.alloc(layout) };
        // 分配失败没有产生新对象，不能增加 live bytes 或成功分配次数。
        if !ptr.is_null() {
            adjust_live_bytes(layout.size() as i128);
            record_allocation(layout.size() as u64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        adjust_live_bytes(-(layout.size() as i128));
        // SAFETY:同 alloc，dealloc 契约由 inner 保证。
        unsafe { self.inner.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY:同 alloc，realloc 契约由 inner 保证。
        let new_ptr = unsafe { self.inner.realloc(ptr, layout, new_size) };
        // realloc 失败时原分配仍然存活，统计必须保持旧尺寸。
        if !new_ptr.is_null() {
            adjust_live_bytes(new_size as i128 - layout.size() as i128);
            record_allocation(new_size as u64);
        }
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

/// scope 退出时恢复 TLS 的 RAII guard。
///
/// 不能只靠 `scope` 末尾的正常返回清理：闭包 panic 时，遗留的 stats 指针会
/// 在栈帧销毁后变成悬垂指针，下一次分配便会解引用它。
struct ObserverReset {
    previous_stats: Option<*mut AllocStats>,
    previous_live: i128,
}

impl Drop for ObserverReset {
    fn drop(&mut self) {
        ALLOC_STATS.with(|cell| cell.set(self.previous_stats));
        LIVE_BYTES.with(|live| {
            // 嵌套 scope 的净分配要并入外层；直接丢弃会让外层随后释放这些对象
            // 时出现负计数。最外层则保留净量，供下一个 scope 清零。
            live.set(self.previous_live + live.get());
        });
        if let Some(stats) = self.previous_stats {
            // SAFETY: previous_stats 只会是仍处于活跃外层 scope 的指针；本 guard
            // 先于内层 stats 栈帧销毁，而外层 guard 尚未运行。
            update_live_high_water(unsafe { &mut *stats });
        }
    }
}

impl AllocObserver {
    pub fn scope<R>(f: impl FnOnce() -> R) -> (R, AllocStats) {
        let previous_stats = ALLOC_STATS.with(Cell::get);
        let previous_live = LIVE_BYTES.with(Cell::get);
        let mut stats = AllocStats::default();
        let stats_ptr: *mut AllocStats = &mut stats;
        ALLOC_STATS.with(|cell| cell.set(Some(stats_ptr)));
        // 每个 scope 独立计量净存活分配；guard 会在退出时恢复并合并外层计数。
        LIVE_BYTES.with(|live| live.set(0));
        let _reset = ObserverReset {
            previous_stats,
            previous_live,
        };
        let result = f();
        (result, stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::alloc::System;

    struct ReallocFails;

    // SAFETY: alloc/dealloc 直接满足 System 的契约；realloc 故意返回 null，
    // 按 GlobalAlloc 契约保留原分配，用于观测失败路径。
    unsafe impl GlobalAlloc for ReallocFails {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            // SAFETY: 布局由调用方按 GlobalAlloc 契约提供。
            unsafe { System.alloc(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            // SAFETY: 指针与布局来自本分配器先前的成功分配。
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, _ptr: *mut u8, _layout: Layout, _new_size: usize) -> *mut u8 {
            std::ptr::null_mut()
        }
    }

    #[test]
    fn panic_restores_the_tls_observer_before_the_stack_frame_dies() {
        let panicked = std::panic::catch_unwind(|| {
            let _ = AllocObserver::scope(|| -> () { panic!("observer regression") });
        });
        assert!(panicked.is_err());
        assert!(ALLOC_STATS.with(Cell::get).is_none());

        // 旧实现会在这里解引用上一段已销毁栈帧里的 stats 指针。
        let (bytes, stats) = AllocObserver::scope(|| vec![1u8; 256]);
        assert_eq!(bytes.len(), 256);
        assert!(stats.bytes >= 256);
    }

    #[test]
    fn nested_scopes_restore_the_outer_observer() {
        let ((), outer) = AllocObserver::scope(|| {
            let (inner_bytes, inner) = AllocObserver::scope(|| vec![1u8; 128]);
            assert_eq!(inner_bytes.len(), 128);
            assert!(inner.bytes >= 128);

            let outer_bytes = vec![2u8; 256];
            assert_eq!(outer_bytes.len(), 256);
        });
        assert!(outer.bytes >= 256);
        assert!(ALLOC_STATS.with(Cell::get).is_none());
    }

    #[test]
    fn failed_realloc_preserves_the_original_live_allocation() {
        let allocator = CountingAllocator {
            inner: ReallocFails,
        };
        let layout = Layout::from_size_align(32, 8).expect("valid layout");
        let (live_after_failure, stats) = AllocObserver::scope(|| {
            // SAFETY: layout 有效；失败后原指针仍归本测试所有，并按原布局释放。
            unsafe {
                let ptr = allocator.alloc(layout);
                assert!(!ptr.is_null());
                let new_ptr = allocator.realloc(ptr, layout, 128);
                assert!(new_ptr.is_null());
                let live = LIVE_BYTES.with(Cell::get);
                allocator.dealloc(ptr, layout);
                live
            }
        });
        assert_eq!(live_after_failure, 32);
        assert_eq!(stats.allocations, 1);
        assert_eq!(stats.bytes, 32);
        assert_eq!(stats.live_bytes_high_water, 32);
    }

    #[test]
    fn successful_realloc_updates_the_high_water_mark() {
        let allocator = CountingAllocator { inner: System };
        let old = Layout::from_size_align(32, 8).expect("valid old layout");
        let new = Layout::from_size_align(128, 8).expect("valid new layout");
        let (live_after_realloc, stats) = AllocObserver::scope(|| {
            // SAFETY: 两个布局有效；成功 realloc 后的指针按新布局释放。
            unsafe {
                let ptr = allocator.alloc(old);
                assert!(!ptr.is_null());
                let ptr = allocator.realloc(ptr, old, new.size());
                assert!(!ptr.is_null());
                let live = LIVE_BYTES.with(Cell::get);
                allocator.dealloc(ptr, new);
                live
            }
        });
        assert_eq!(live_after_realloc, 128);
        assert_eq!(stats.allocations, 2);
        assert_eq!(stats.bytes, 160);
        assert_eq!(stats.live_bytes_high_water, 128);
    }
}
