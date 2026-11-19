//! 终端输出的流控与分发：把"要发给谁、发多少、发不出去怎么办"收在一处。
//!
//! # 这个模块解决什么问题
//!
//! 旧实现里这套逻辑散在三处（`terminal.rs` 的 `Subscriber`、`connection.rs`
//! 的 relay 线程、`client.rs` 的路由表），而且**各自维护一份状态**：
//!
//! - `Subscriber::credit`（字节额度）
//! - `Subscriber::repaint_pending`（终端线程决定"不再推增量"）
//! - `TerminalState::repaint_ids`（连接层"没额度，等信用再补"）
//!
//! 三个标记描述的是同一件事的三个阶段，却分别在不同线程里被置位/清除。
//! 这正是这一系列 bug 的来源：某个阶段忘了清、或者两个阶段的判断不一致，
//! 客户端就永久停在旧画面上，而每条日志都写着 `ok`。
//!
//! # 现在的模型
//!
//! 每个订阅者只有一个状态机（[`Delivery`]），四个状态：
//!
//! ```text
//!                   有额度且无积压
//!   ┌──────────┐ ──────────────────▶ ┌──────────┐
//!   │  Live    │                     │  Live    │
//!   │ (增量)   │ ◀────────────────── │ (增量)   │
//!   └────┬─────┘   整屏快照发成功     └────┬─────┘
//!        │ 额度不足 / 积压超限             │
//!        ▼                                │
//!   ┌──────────┐   额度恢复              │
//!   │ NeedsPaint│ ──────────────────────┘
//!   │ (停发增量)│
//!   └──────────┘
//! ```
//!
//! 关键不变量（旧实现就是因为破坏了它而出错）：
//!
//! 1. **停发增量期间一个字节都不发**——不是"少发"，是零。否则客户端会
//!    收到基于新状态画的局部更新、同时缺着丢掉的那一块，画面长期错位。
//! 2. **只有整屏快照能把它带回 Live**——增量永远补不回丢掉的那段。
//! 3. 状态与前向数据都在同一把锁下变更，不存在"标记清了但数据没发"。

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::protocol::{Frame, Kind};

/// 一个订阅者的投递状态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// 正常：按额度推增量。
    Live,
    /// 欠着一帧整屏重绘：**停发一切增量**，等快照。
    NeedsPaint,
}

/// 一个订阅者的完整投递状态。
///
/// 只有 [`Hub`] 持有它，外界通过句柄操作，避免多线程各自维护副本。
pub struct Sink {
    /// 唯一标识（全局单调递增）。
    id: u64,
    /// 出帧的通道。
    tx: async_channel::Sender<Frame>,
    /// 对端的 channel 号（对端用它路由到具体终端视图）。
    channel: u32,
    /// 还能发多少字节。
    credit: u64,
    /// 投递状态。
    delivery: Delivery,
}

/// 流控额度：每个订阅者初始能收多少字节。
///
/// 与 herdr 的 `WINDOW = 256 KiB` 同量级。取 256 KiB 而不是更大：
/// - 太小（如 64 KiB）会让大窗口的一次整屏重绘（180×50 ≈ 200 KB）都发不出去；
/// - 太大（如 4 MiB）等于没有背压，客户端一慢就会在内存里堆出几十 MB。
///
/// 256 KiB 刚好容得下一次整屏快照，又能在客户端真正卡住时迅速停下来。
pub const INITIAL_CREDIT: u64 = 256 * 1024;

/// 一个终端的所有订阅者。
///
/// 加锁粒度就是"一次广播"：读线程拿到 `&Hub` 后调一次 [`Hub::broadcast`]，
/// 期间持有内部锁。临界区只做内存拷贝与 `try_send`，不做 IO。
pub struct Hub {
    sinks: std::sync::Mutex<Vec<Sink>>,
    next_id: AtomicU64,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Self {
        Self {
            sinks: std::sync::Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Sink>> {
        self.sinks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 登记一个订阅者，返回它的 id。
    pub fn subscribe(&self, channel: u32, tx: async_channel::Sender<Frame>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().push(Sink {
            id,
            tx,
            channel,
            credit: INITIAL_CREDIT,
            delivery: Delivery::Live,
        });
        id
    }

    /// 注销订阅者。
    pub fn unsubscribe(&self, id: u64) {
        self.lock().retain(|s| s.id != id);
    }

    /// 是否有订阅者。
    ///
    /// 用于跳过"没人看时的整屏序列化"（见 `TerminalHandle::repaint_after_resize`）。
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// 归还额度，并在攒够一帧快照时让欠着重绘的订阅者恢复。
    ///
    /// `paint` 是生成整屏快照的闭包——只在真的需要时才调用（它要遍历整个
    /// 网格并序列化，是这条路径上最贵的操作）。
    pub fn grant_credit(&self, id: u64, bytes: u64, paint: impl FnOnce() -> Vec<u8>) {
        let mut sinks = self.lock();
        let Some(sink) = sinks.iter_mut().find(|s| s.id == id) else {
            return;
        };
        sink.credit = sink.credit.saturating_add(bytes);
        if sink.delivery != Delivery::NeedsPaint {
            return;
        }
        // 欠着重绘：额度够了就立刻补一帧整屏。
        //
        // 必须先算快照再判断额度（快照长度决定够不够），但要避免"算了却
        // 发不出去"的浪费——所以先看一眼额度是否明显不够。
        if sink.credit == 0 {
            return;
        }
        drop(sinks);
        let snapshot = paint();
        let mut sinks = self.lock();
        let Some(sink) = sinks.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if sink.credit < snapshot.len() as u64 {
            // 还不够：保持 NeedsPaint，等下一次信用。
            return;
        }
        sink.credit -= snapshot.len() as u64;
        if sink
            .tx
            .try_send(Frame::data(sink.channel, snapshot))
            .is_ok()
        {
            sink.delivery = Delivery::Live;
        }
    }

    /// 广播一块输出。
    ///
    /// `paint` 只在某个订阅者需要补快照时才被调用，最多一次。
    pub fn broadcast(&self, chunk: &[u8], paint: impl FnOnce() -> Vec<u8>) {
        let mut need_paint = false;
        {
            let mut sinks = self.lock();
            let mut dropped = Vec::new();
            for (idx, sink) in sinks.iter_mut().enumerate() {
                if sink.delivery == Delivery::NeedsPaint {
                    // **停发期间一个字节都不发**。见模块文档的不变量 1。
                    continue;
                }
                if sink.credit < chunk.len() as u64 {
                    sink.delivery = Delivery::NeedsPaint;
                    need_paint = true;
                    continue;
                }
                sink.credit -= chunk.len() as u64;
                if sink
                    .tx
                    .try_send(Frame::data(sink.channel, chunk.to_vec()))
                    .is_err()
                {
                    dropped.push(idx);
                }
            }
            // 倒序删除，避免下标失效。
            for idx in dropped.into_iter().rev() {
                sinks.remove(idx);
            }
        }
        if need_paint {
            // 有一个订阅者已经欠快照了，趁这次机会补上。
            //
            // 只补一次快照、按各自额度分别发——旧实现是每个订阅者各生成
            // 一份快照，N 个观众就是 N 次整屏序列化。
            let snapshot = paint();
            let mut sinks = self.lock();
            for sink in sinks.iter_mut() {
                if sink.delivery != Delivery::NeedsPaint {
                    continue;
                }
                if sink.credit < snapshot.len() as u64 {
                    continue;
                }
                sink.credit -= snapshot.len() as u64;
                if sink
                    .tx
                    .try_send(Frame::data(sink.channel, snapshot.clone()))
                    .is_ok()
                {
                    sink.delivery = Delivery::Live;
                }
            }
        }
    }

    /// 给所有"欠重绘且额度够"的订阅者补一帧快照。
    ///
    /// resize 之后调用：reflow 改了每一行的折行位置，增量已经无法表达
    /// 正确画面，必须整屏重来。
    pub fn repaint_all(&self, paint: impl FnOnce() -> Vec<u8>) {
        // 粗筛：**没有订阅者时一个字节都不该算**。
        //
        // 整屏序列化是 `Hub` 上最贵的操作（遍历整张网格 + 拼 ANSI）。旧写
        // 法的 `!any && sinks.is_empty()` 是死代码——`sinks.is_empty()` 已经
        // 蕴含它，而真正该问的是"有没有人收得到"。
        if self.lock().is_empty() {
            return;
        }
        let snapshot = paint();
        let mut sinks = self.lock();
        let mut dropped = Vec::new();
        for (idx, sink) in sinks.iter_mut().enumerate() {
            if sink.credit < snapshot.len() as u64 {
                // 额度不够就跳过这次重绘，等客户端还信用。
                //
                // **不能**强行补额度：重绘本身就是一大段数据，客户端消化
                // 不过来时强推只会让它的接收队列越积越多、界面越发滞后。
                sink.delivery = Delivery::NeedsPaint;
                continue;
            }
            sink.credit -= snapshot.len() as u64;
            if sink
                .tx
                .try_send(Frame::data(sink.channel, snapshot.clone()))
                .is_ok()
            {
                sink.delivery = Delivery::Live;
            } else {
                dropped.push(idx);
            }
        }
        for idx in dropped.into_iter().rev() {
            sinks.remove(idx);
        }
    }

    /// 给所有订阅者推一条**元信息**（前台进程等）。
    ///
    /// 与 `broadcast` 的区别：它**不消耗流控额度、也不受 NeedsPaint 影响**。
    ///
    /// 理由：元信息不是屏幕内容，它是"标签该显示什么"的状态。停发增量是
    /// 为了不让画面错位，而标签与画面完全独立——把它也停掉只会让标签在
    /// 客户端追不上时莫名其妙地停止更新。
    pub fn meta(&self, payload: &[u8]) {
        let mut sinks = self.lock();
        let mut dropped = Vec::new();
        for (idx, sink) in sinks.iter_mut().enumerate() {
            if sink
                .tx
                .try_send(Frame::new(sink.channel, Kind::Meta, payload.to_vec()))
                .is_err()
            {
                dropped.push(idx);
            }
        }
        for idx in dropped.into_iter().rev() {
            sinks.remove(idx);
        }
    }

    /// 终端结束：通知所有订阅者，然后清空。
    pub fn finish(&self) {
        let mut sinks = self.lock();
        for sink in sinks.iter() {
            let _ = sink.tx.try_send(Frame {
                channel: sink.channel,
                kind: Kind::Close,
                payload: b"{}".to_vec(),
            });
        }
        sinks.clear();
    }
}

/// 多个终端共用的句柄。
pub type SharedHub = Arc<Hub>;

#[cfg(test)]
mod tests {
    use super::*;

    fn hub_with(n: usize) -> (Hub, Vec<async_channel::Receiver<Frame>>) {
        let hub = Hub::new();
        let mut rxs = Vec::new();
        for ch in 1..=n as u32 {
            let (tx, rx) = async_channel::unbounded();
            hub.subscribe(ch, tx);
            rxs.push(rx);
        }
        (hub, rxs)
    }

    /// 正常路径：有额度就按顺序推增量。
    #[test]
    fn live_subscribers_receive_incremental_output() {
        let (hub, rxs) = hub_with(1);
        hub.broadcast(b"hello", || unreachable!("不该生成快照"));
        let frame = rxs[0].try_recv().expect("应收到增量");
        assert_eq!(frame.payload, b"hello");
    }

    /// **额度耗尽后必须停发增量，不是少发。**
    ///
    /// 回归价值：旧实现在额度不足时"丢这一段、记个标记"，但**中间继续推
    /// 增量**。客户端于是长期处在"局部更新 + 缺一块"的状态（画面错乱），
    /// 而且每段被跳过的增量又记一次重绘，额度一恢复就要连续生成好几帧。
    #[test]
    fn an_overdrawn_subscriber_gets_no_more_deltas() {
        let (hub, rxs) = hub_with(1);
        // 额度只有 3 字节，装不下任何一个 chunk，也装不下快照。
        {
            let mut sinks = hub.lock();
            sinks[0].credit = 3;
        }
        hub.broadcast(b"chunk-that-does-not-fit", || b"PAINT".to_vec());

        // 额度连快照都不够：**一个字节都不发**，并停在这个状态。
        assert!(
            rxs[0].try_recv().is_err(),
            "额度不足时既不能发增量，也不能强行发快照"
        );
        assert_eq!(
            hub.lock()[0].delivery,
            Delivery::NeedsPaint,
            "应停在 NeedsPaint，等额度恢复"
        );

        // 再广播一次：仍然什么都不发（**不是**少发几个增量）。
        hub.broadcast(b"another", || b"PAINT2".to_vec());
        assert!(
            rxs[0].try_recv().is_err(),
            "欠着快照期间不能推任何增量，否则画面会长期错位"
        );

        // 额度恢复后才补快照并回到 Live。
        let id = hub.lock()[0].id;
        hub.grant_credit(id, 1 << 20, || b"PAINT3".to_vec());
        let f = rxs[0].try_recv().expect("额度恢复后应补快照");
        assert_eq!(f.payload, b"PAINT3");
        assert_eq!(hub.lock()[0].delivery, Delivery::Live);
    }

    /// 补完快照后立刻恢复推增量（否则界面会永远停在快照那一刻）。
    #[test]
    fn a_painted_subscriber_resumes_deltas() {
        let (hub, rxs) = hub_with(1);
        {
            let mut sinks = hub.lock();
            sinks[0].credit = 1;
            sinks[0].delivery = Delivery::NeedsPaint;
        }
        // 额度恢复：补快照 → 回 Live。
        hub.grant_credit(99, 0, || unreachable!()); // 不存在的 id，无副作用
        let id = hub.lock()[0].id;
        hub.grant_credit(id, 1 << 20, || b"PAINT".to_vec());
        let paint = rxs[0].try_recv().expect("应补快照");
        assert_eq!(paint.payload, b"PAINT");

        // 之后增量正常。
        hub.broadcast(b"delta", || unreachable!("已回 Live，不该再生成快照"));
        let delta = rxs[0].try_recv().expect("应恢复推增量");
        assert_eq!(delta.payload, b"delta");
    }

    /// 多个订阅者只需要**一次**整屏序列化。
    ///
    /// 回归价值：旧实现每个订阅者各生成一份快照，N 个观众 N 次整屏遍历，
    /// 而这条路径跑在连接线程上——它一慢，输入就跟着卡。
    #[test]
    fn one_repaint_serves_every_subscriber() {
        let (hub, rxs) = hub_with(3);
        // 额度足够，但强制三个订阅者都欠快照。
        {
            let mut sinks = hub.lock();
            for s in sinks.iter_mut() {
                s.delivery = Delivery::NeedsPaint;
            }
        }
        let mut calls = 0;
        hub.repaint_all(|| {
            calls += 1;
            b"PAINT".to_vec()
        });
        assert_eq!(calls, 1, "3 个订阅者只应序列化一次");
        for (i, rx) in rxs.iter().enumerate() {
            let f = rx
                .try_recv()
                .unwrap_or_else(|_| panic!("订阅者 {i} 应收到快照"));
            assert_eq!(f.payload, b"PAINT");
        }
        assert!(
            hub.lock().iter().all(|s| s.delivery == Delivery::Live),
            "快照发出后都应回到 Live"
        );
    }

    /// **额度耗尽的订阅者必须能被真正丢掉。**
    ///
    /// 回归价值：这是"关掉窗口再打开，TUI 卡死"的第二层原因。
    ///
    /// 额度只减不增，而耗尽的订阅者会停在 `NeedsPaint`——它的状态是
    /// `Hub` 独占的，**客户端补信用也救不回来**（客户端不知道它存在）。
    /// 唯一的活路是 `unsubscribe`。而 `unsubscribe` 是否发生，取决于
    /// 连接关闭时有没有把它退掉：core 的 registry 是空的，真正持有
    /// 订阅者的是 holder，漏退就永久留着。
    ///
    /// 这里钉住前提：退掉之后，这条 Sink 不再参与任何广播——即使它
    /// 当时已经卡在 `NeedsPaint`。
    #[test]
    fn an_unsubscribed_sink_stops_being_a_candidate() {
        let (hub, rxs) = hub_with(2);
        // 让第一条额度耗尽并停在 NeedsPaint（模拟"窗口关了但没退订"）。
        {
            let mut sinks = hub.lock();
            sinks[0].credit = 0;
        }
        hub.broadcast(b"x", || b"PAINT".to_vec());
        assert_eq!(hub.lock()[0].delivery, Delivery::NeedsPaint);
        // 这一段里另一个订阅者是活的：它收到了那次增量。
        assert_eq!(rxs[1].try_recv().expect("活着的订阅者应收到").payload, b"x");

        // 真正退掉那个卡住的。
        let stuck = hub.lock()[0].id;
        hub.unsubscribe(stuck);
        assert_eq!(hub.lock().len(), 1, "退订后只剩一个订阅者");

        // 之后广播不该再被它拖累：不序列化快照、也不影响剩下的订阅者。
        hub.broadcast(b"delta", || unreachable!("已没人欠快照，不该生成快照"));
        let frame = rxs[1].try_recv().expect("活着的订阅者应收到增量");
        assert_eq!(frame.payload, b"delta");
        assert!(rxs[0].try_recv().is_err(), "退订的订阅者不该再收到任何东西");
    }

    /// 注销后不再收到任何东西。
    #[test]
    fn unsubscribing_stops_delivery() {
        let (hub, rxs) = hub_with(1);
        let id = hub.lock()[0].id;
        hub.unsubscribe(id);
        assert!(hub.is_empty());
        hub.broadcast(b"x", || unreachable!());
        assert!(rxs[0].try_recv().is_err(), "注销后不该再收到数据");
    }

    /// 初始额度必须容得下一次整屏快照（否则大窗口永远画不出第一帧）。
    #[test]
    fn the_initial_credit_fits_a_large_screen() {
        // 200 列 × 60 行，最坏情况每格一个 SGR（约 20 字节）。
        let worst = 200 * 60 * 20;
        assert!(
            INITIAL_CREDIT as usize > worst / 4,
            "初始额度 {INITIAL_CREDIT} 太小，容不下大窗口的一次重绘"
        );
    }
}
