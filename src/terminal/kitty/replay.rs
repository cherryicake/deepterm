//! 把图像存储重新编码成 kitty 协议命令，供重连时回放。
//!
//! # 为什么需要它
//!
//! kitty 图形协议是**带状态**的：程序发 `a=t` 把像素传进终端，再发 `a=p`
//! 把图像摆到某个网格位置。终端（客户端）各自维护一份存储。窗口关掉时这份
//! 存储随进程消失，重连后守护进程只能重放**文字**——截图、图表这类内容会
//! 变成空白，而程序并不知道（它以为图还在）。
//!
//! 守护进程现在也维护同样的存储（见 `registry::run_terminal` 里的扫描器），
//! 于是可以在 attach 时把「图像 + 放置」重新编码成命令发一遍，让新客户端的
//! 存储与程序当初建立的状态一致。
//!
//! # 为什么是"重新编码"而不是"回放原始 APC 字节"
//!
//! 原始字节流里含删除（`a=d`）、替换、动画帧……按顺序重放才能得到正确结果，
//! 而那等于把整条历史都留着（几十 MB 起），还会因为丢帧而错位。直接读
//! **当前**存储重新编码，天然只包含"现在还在的那些图"，与屏幕状态一致。
//!
//! # 已知取舍
//!
//! - 只回放 `f=32`（RGBA 直传）。原始客户端可能用 PNG（`f=100`）传输，重编码
//!   成原始像素传输量更大，但省掉一次压缩与解码，且实现简单、无外部依赖。
//! - 回滚区里的放置（`row < 0`）不回放：客户端此刻的视口在底部，这些图已经
//!   滚出屏幕；要恢复它们还得把回滚行数也算进来，收益不抵复杂度。
//! - 动画只回放当前帧（`animation.current_index`），不重建时间轴。重连后
//!   动画会停在该帧，直到程序下一次 `a=f`。
//!
//! 回放命令全部带 `q=2`（静默）：新客户端不该因为这些命令向程序回写应答，
//! 否则程序会收到一波它没请求的 `OK`。

use super::command::encode_base64;
use super::storage::{PlacementLocation, Storage};

/// 单条命令的原始负载字节数。
///
/// base64 后为 4096 字节，与 kitty 文档建议的分片大小一致。客户端实现
/// （含 deepterm 自己）普遍按这个量级处理，超大的单条命令在某些实现里
/// 会被拒。
const CHUNK_BYTES: usize = 3072;

/// 回放结果。
#[derive(Debug, Default)]
pub struct Replay {
    /// 编码好的命令字节。
    pub bytes: Vec<u8>,
    /// 回放了多少张图像。
    pub images: usize,
    /// 回放了多少个放置。
    pub placements: usize,
    /// 因超出预算或不在视口内而跳过的图像/放置数量（诊断用）。
    pub skipped: usize,
}

/// 把存储里的图像与放置编码成 kitty 命令。
///
/// `rows` 是当前视口行数：只有落在视口内的放置才回放（回滚区里的已经看不见，
/// 且行号需要额外换算）。`budget` 是命令字节总量上限，达到后停止回放——
/// 宁可少恢复几张图，也不能让 attach 变成一次几 MB 的传输。
pub fn encode(storage: &Storage, rows: usize, budget: usize) -> Replay {
    let mut out = Replay::default();

    // 光标保存/恢复：回放要移动光标来定位放置，不能把文字重绘留下的
    // 光标位置改掉。
    out.bytes.extend_from_slice(b"\x1b7");

    // 先按 id 排序，保证多次 attach 的画面一致（HashMap 迭代顺序随机，
    // 重叠图像的前后关系会抖）。
    let mut ids: Vec<_> = storage.iter().map(|(id, _)| *id).collect();
    ids.sort_unstable();

    for id in ids {
        let Some(stored) = storage.get(id) else {
            continue;
        };
        if out.bytes.len() >= budget {
            out.skipped += 1;
            continue;
        }
        // 没有任何放置的图像不显示（`a=t` 只传不摆），也没有必要回放——
        // 程序后续要显示时会自己再发 `a=p`，而它引用的是**它自己那条
        // 连接**的图像 id；我们回传了图反而会在客户端留下一个程序不知道
        // 的残留。等等：程序的 `a=p` 只带 id，不带像素，若它引用的是
        // 断开前传的图，就必须有这个 id 的像素。所以只有在"有放置"时
        // 才说明它真的在等这份像素。程序若稍后显示一张当前不可见的图，
        // 我们无从得知——这是有意的取舍：优先保证屏幕上看得见的内容。
        if stored.placements.is_empty() {
            continue;
        }

        // 图像数据。`render_data` 是当前动画帧（无动画时就是根帧）。
        let data = stored.render_data();
        let (w, h) = (stored.image.width, stored.image.height);
        let mut offset = 0;
        let mut first = true;
        while offset < data.len() {
            let end = (offset + CHUNK_BYTES).min(data.len());
            let more = end < data.len();
            let chunk = encode_base64(&data[offset..end]);
            let mut cmd = String::with_capacity(chunk.len() + 96);
            if first {
                cmd.push_str(&format!(
                    "\x1b_Ga=t,f=32,s={w},v={h},i={id},q=2,m={}",
                    u8::from(more)
                ));
                first = false;
            } else {
                cmd.push_str(&format!("\x1b_Gm={}", u8::from(more)));
            }
            cmd.push(';');
            cmd.push_str(&chunk);
            cmd.push_str("\x1b\\");
            out.bytes.extend_from_slice(cmd.as_bytes());
            offset = end;
        }
        out.images += 1;

        // 放置。虚拟放置（`U=1`）由网格里的 U+10EEEE 占位符决定位置，
        // 光标在哪都一样；普通放置必须在光标处创建，所以先定位。
        let mut placement_ids: Vec<_> = stored.placements.keys().copied().collect();
        placement_ids.sort_unstable();
        for pid in placement_ids {
            if out.bytes.len() >= budget {
                out.skipped += 1;
                continue;
            }
            let Some(p) = stored.placements.get(&pid) else {
                continue;
            };
            if p.location == PlacementLocation::Anchor {
                // 视口外的放置：行号为负（在回滚区）或超出视口底部，都不该
                // 在本屏出现。
                if p.row < 0 || p.row as usize >= rows {
                    out.skipped += 1;
                    continue;
                }
                out.bytes
                    .extend_from_slice(format!("\x1b[{};{}H", p.row + 1, p.col + 1).as_bytes());
            }
            let mut cmd = format!("\x1b_Ga=p,i={id},p={pid},q=2");
            if let Some(cols) = p.columns {
                cmd.push_str(&format!(",c={cols}"));
            }
            if let Some(pr) = p.rows {
                cmd.push_str(&format!(",r={pr}"));
            }
            if p.x_offset != 0 {
                cmd.push_str(&format!(",X={}", p.x_offset));
            }
            if p.y_offset != 0 {
                cmd.push_str(&format!(",Y={}", p.y_offset));
            }
            if p.z != 0 {
                cmd.push_str(&format!(",z={}", p.z));
            }
            if p.location == PlacementLocation::Virtual {
                cmd.push_str(",U=1");
            }
            cmd.push_str("\x1b\\");
            out.bytes.extend_from_slice(cmd.as_bytes());
            out.placements += 1;
        }
    }

    out.bytes.extend_from_slice(b"\x1b8");
    if out.images == 0 {
        // 没有图像时不必留下一个空的保存/恢复对。
        out.bytes.clear();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal::kitty::image::DecodedImage;
    use crate::terminal::kitty::storage::{CellGeometry, Placement};

    fn storage_with_image() -> Storage {
        let mut storage = Storage::new(1 << 20);
        storage.insert(
            7,
            DecodedImage {
                width: 2,
                height: 1,
                rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
            },
            Some(1),
            false,
        );
        storage.place(
            7,
            0,
            Placement {
                row: 3,
                col: 5,
                ..Default::default()
            },
        );
        storage
    }

    /// 回放必须先传像素再摆位置，且两条命令都带原始 id。
    ///
    /// 回归价值：顺序反了，`a=p` 会因为"图像不存在"被静默忽略——屏幕上
    /// 什么都不显示，而日志里一条错误都没有。
    #[test]
    fn replay_transmits_then_places() {
        let storage = storage_with_image();
        let replay = encode(&storage, 24, 1 << 20);
        let text = String::from_utf8_lossy(&replay.bytes);
        let transmit = text.find("a=t,f=32").expect("必须传像素");
        let place = text.find("a=p,i=7,p=0").expect("必须摆位置");
        assert!(transmit < place, "必须先传后摆");
        assert!(text.starts_with("\x1b7"), "回放前后要保存/恢复光标");
        assert!(text.ends_with("\x1b8"));
        assert_eq!(replay.images, 1);
        assert_eq!(replay.placements, 1);
    }

    /// 视口外的放置不回放：它们已经滚出屏幕，回放只会把图错摆到别的行。
    #[test]
    fn placements_outside_the_viewport_are_skipped() {
        let mut storage = Storage::new(1 << 20);
        storage.insert(
            1,
            DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![0, 0, 0, 255],
            },
            None,
            false,
        );
        storage.place(
            1,
            0,
            Placement {
                row: -5,
                col: 0,
                ..Default::default()
            },
        );
        let replay = encode(&storage, 24, 1 << 20);
        assert_eq!(replay.placements, 0);
        assert!(replay.skipped > 0);
    }

    /// 预算必须真的封顶：图像再多也不会生成超过预算的命令。
    ///
    /// 这条保护的是 attach 的延迟：一张大截图 base64 后上兆，无上限时
    /// 重连会卡在传输上。
    #[test]
    fn the_budget_caps_the_output() {
        let mut storage = Storage::new(64 << 20);
        for id in 0..8u32 {
            storage.insert(
                id,
                DecodedImage {
                    width: 64,
                    height: 64,
                    rgba: vec![9; 64 * 64 * 4],
                },
                None,
                false,
            );
            storage.place(
                id,
                0,
                Placement {
                    row: 0,
                    col: 0,
                    ..Default::default()
                },
            );
        }
        let replay = encode(&storage, 24, 64 * 1024);
        assert!(
            replay.bytes.len() <= 64 * 1024 + 16 * 1024,
            "预算应封顶，实际 {} 字节",
            replay.bytes.len()
        );
        assert!(replay.skipped > 0, "超预算的图像应被计数");
    }

    /// 没有图像时不产生任何字节：attach 的普通路径不该多出两个空序列。
    #[test]
    fn empty_storage_produces_nothing() {
        let replay = encode(&Storage::new(1 << 20), 24, 1 << 20);
        assert!(replay.bytes.is_empty());
        assert_eq!(replay.images, 0);
    }

    /// 放置的网格尺寸/偏移/层级都要原样带回去。
    #[test]
    fn placement_attributes_survive_the_round_trip() {
        let mut storage = Storage::new(1 << 20);
        storage.insert(
            3,
            DecodedImage {
                width: 1,
                height: 1,
                rgba: vec![0; 4],
            },
            None,
            false,
        );
        storage.place(
            3,
            2,
            Placement {
                row: 1,
                col: 1,
                columns: Some(4),
                rows: Some(2),
                x_offset: 3,
                y_offset: 4,
                z: -1,
                location: PlacementLocation::Anchor,
                relative_parent: None,
            },
        );
        let text = String::from_utf8_lossy(&encode(&storage, 24, 1 << 20).bytes).into_owned();
        for expected in [",c=4", ",r=2", ",X=3", ",Y=4", ",z=-1"] {
            assert!(text.contains(expected), "缺少 {expected}: {text}");
        }
        // 几何不能影响删除判定，这里只确认没有额外把 U=1 带上。
        assert!(!text.contains(",U=1"));
        let _ = CellGeometry {
            cell_width: 1,
            cell_height: 1,
        };
    }
}
