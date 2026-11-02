//! Kitty 图形协议实现。
//!
//! 分层对齐 ghostty `terminal/kitty/`：
//! - [`command`]：APC 字节流 → 结构化命令（`k=v` 控制对 + base64 负载）；
//! - [`image`]：分片累积与像素解码（RGB/RGBA/PNG）；
//! - [`storage`]：图像与放置（placement）的存储、上限与淘汰；
//! - [`animation`]：动画帧模型、像素合成与定时推进；
//! - [`unicode`]：Unicode 占位符（U+10EEEE）的解码与连续段合并；
//! - [`exec`]：命令执行与响应生成。

pub mod animation;
pub mod clipboard;
pub mod command;
pub mod exec;
pub mod image;
pub mod render;
pub mod scanner;
pub mod storage;
pub mod unicode;

pub use animation::{Animation, Background, CompositionMode, DEFAULT_GAP_MS, Frame};
pub use command::{Action, Command, ControlPairs, DeleteTarget, Format, Medium, Quiet};
pub use exec::{ExecutionError, ImageStore, MediumPolicy, Response};
pub use image::{DecodedImage, ImageId};
pub use render::{
    AnimationClock, CellMetrics, ImageCache, PlacedImage, PlaceholderCell, SourceRect, Viewport,
};
pub use scanner::{CollectingSink, KittyGraphicsScanner, ReplySink};
pub use storage::{Placement, PlacementKey, PlacementLocation};
pub use unicode::{IncompletePlacement, VirtualPlacement};
