//! 语言服务器的实时状态，供编辑器状态栏显示。
//!
//! 状态来源有两路：
//! - **生命周期**：`LspManager` 启动进程 → `initialize` 握手 → 就绪/失败；
//! - **工作进度**：服务器通过 `$/progress` 通知推送（rust-analyzer 的
//!   "Indexing"、"Roots Scanned" 等），带 `title` / `message` / `percentage`。
//!
//! 一个服务器会同时开多个进度令牌（索引、扫描工作区、拉取依赖…），所以这里
//! 维护一张 `token → 进度` 表，展示时挑一个最有信息量的（有百分比的优先）。

use std::collections::HashMap;

use lsp_types::{ProgressToken, WorkDoneProgress};

/// 语言服务器当前状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LspStatus {
    /// 正在启动：进程已 spawn，`initialize` 握手中。
    Starting,
    /// 服务器正在工作（分析 / 索引），可能带进度。
    Working {
        /// 阶段标题，如 "Indexing"。
        title: String,
        /// 更细的说明，如 "3/25 files"。
        message: Option<String>,
        /// 0-100；服务器不一定给。
        percentage: Option<u32>,
    },
    /// 就绪：握手完成且没有进行中的进度。
    ///
    /// 带上诊断计数，这就是「实时状态」——不打开问题面板也能一眼看到
    /// 当前文件有几个错误/警告。
    Ready { errors: usize, warnings: usize },
    /// 启动失败或进程已退出。
    Failed { reason: String },
    /// 用户主动停止：进程没在跑，直到用户点「启动」/「重启」。
    ///
    /// 与 [`Self::Failed`] 分开是因为语义完全不同：失败是意外（红），
    /// 停止是用户的选择（灰）。混成一个态会让用户以为自己的操作搞坏了
    /// 什么，也会让菜单无法判断该把「启动」还是「停止」置灰。
    Stopped,
}

impl LspStatus {
    /// 是否处于「进行中」——需要画转圈动画。
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Starting | Self::Working { .. })
    }

    /// 状态栏上的短标签。
    pub fn label(&self) -> String {
        match self {
            Self::Starting => "启动中".to_string(),
            Self::Working {
                title,
                message,
                percentage,
            } => {
                // 百分比优先拼在标题后；没有就退到 message。
                let mut label = title.clone();
                if let Some(p) = percentage {
                    label.push_str(&format!(" {p}%"));
                } else if let Some(m) = message.as_deref().filter(|m| !m.is_empty()) {
                    label.push_str(&format!(" {m}"));
                }
                label
            }
            Self::Ready { errors, warnings } => match (errors, warnings) {
                (0, 0) => "就绪".to_string(),
                (0, w) => format!("{w} 个警告"),
                (e, 0) => format!("{e} 个错误"),
                (e, w) => format!("{e} 错误 {w} 警告"),
            },
            Self::Failed { reason } => format!("LSP 失败: {reason}"),
            Self::Stopped => "已停止".to_string(),
        }
    }
}

/// 一个进行中的进度令牌。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveProgress {
    title: String,
    message: Option<String>,
    percentage: Option<u32>,
}

/// `$/progress` 的累计状态。
///
/// 单独成类型便于单测：服务器同时开多个令牌，begin/report/end 会交错到达。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProgressTracker {
    active: HashMap<String, ActiveProgress>,
}

/// 令牌 → 字符串键（`ProgressToken` 可能是数字或字符串）。
fn token_key(token: &ProgressToken) -> String {
    match token {
        ProgressToken::Number(n) => n.to_string(),
        ProgressToken::String(s) => s.clone(),
    }
}

impl ProgressTracker {
    /// 是否还有进行中的进度。
    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }

    /// 应用一条 `$/progress` 通知。
    ///
    /// 返回 false 表示这条通知不影响状态（未知形态），调用方可以跳过重绘。
    pub fn apply(&mut self, token: &ProgressToken, value: &WorkDoneProgress) -> bool {
        let key = token_key(token);
        match value {
            WorkDoneProgress::Begin(begin) => {
                self.active.insert(
                    key,
                    ActiveProgress {
                        title: begin.title.clone(),
                        message: begin.message.clone(),
                        percentage: begin.percentage,
                    },
                );
                true
            }
            WorkDoneProgress::Report(report) => {
                // 服务器可能只发 report 不发 begin（少见但合法）：此时按
                // 空标题建一个，总比丢掉进度好。
                let entry = self.active.entry(key).or_insert_with(|| ActiveProgress {
                    title: String::new(),
                    message: None,
                    percentage: None,
                });
                // `message` 缺省表示「沿用上一次」，不能覆盖成 None。
                if report.message.is_some() {
                    entry.message = report.message.clone();
                }
                if report.percentage.is_some() {
                    entry.percentage = report.percentage;
                }
                true
            }
            WorkDoneProgress::End(_) => {
                // 令牌结束：删掉。未知令牌的 end 也算「有变化」但不影响展示。
                self.active.remove(&key);
                true
            }
        }
    }

    /// 挑一条最能代表当前工作的进度。
    ///
    /// 优先带百分比的（用户最关心「还要多久」），其次标题非空的，
    /// 最后任意一条。同优先级下按令牌名排序，保证多次渲染结果稳定
    /// （HashMap 迭代顺序不定，不排序会导致标签闪烁）。
    pub fn representative(&self) -> Option<(String, Option<String>, Option<u32>)> {
        let mut items: Vec<(&String, &ActiveProgress)> = self.active.iter().collect();
        items.sort_by(|a, b| a.0.cmp(b.0));

        let pick = items
            .iter()
            .find(|(_, p)| p.percentage.is_some())
            .or_else(|| items.iter().find(|(_, p)| !p.title.is_empty()))
            .or_else(|| items.first())?;

        Some((
            pick.1.title.clone(),
            pick.1.message.clone(),
            pick.1.percentage,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressReport};

    fn token(s: &str) -> ProgressToken {
        ProgressToken::String(s.to_string())
    }

    fn begin(title: &str, pct: Option<u32>) -> WorkDoneProgress {
        WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: title.to_string(),
            cancellable: None,
            message: None,
            percentage: pct,
        })
    }

    fn report(msg: Option<&str>, pct: Option<u32>) -> WorkDoneProgress {
        WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: None,
            message: msg.map(|s| s.to_string()),
            percentage: pct,
        })
    }

    fn end() -> WorkDoneProgress {
        WorkDoneProgress::End(WorkDoneProgressEnd { message: None })
    }

    #[test]
    fn begin_then_report_tracks_title_and_percentage() {
        let mut t = ProgressTracker::default();
        assert!(t.apply(&token("ra/Indexing"), &begin("Indexing", Some(0))));
        assert!(t.apply(&token("ra/Indexing"), &report(Some("3/25 files"), Some(12))));

        let (title, msg, pct) = t.representative().unwrap();
        assert_eq!(title, "Indexing");
        assert_eq!(msg.as_deref(), Some("3/25 files"));
        assert_eq!(pct, Some(12));
        assert!(!t.is_empty());
    }

    #[test]
    fn end_clears_the_token() {
        let mut t = ProgressTracker::default();
        t.apply(&token("a"), &begin("Indexing", None));
        assert!(!t.is_empty());
        t.apply(&token("a"), &end());
        assert!(t.is_empty(), "end must remove the token");
        assert!(t.representative().is_none());
    }

    #[test]
    fn report_without_begin_still_tracked() {
        // 合法但少见：没有 begin 直接 report。
        let mut t = ProgressTracker::default();
        t.apply(&token("x"), &report(Some("working"), Some(50)));
        let (_, msg, pct) = t.representative().unwrap();
        assert_eq!(msg.as_deref(), Some("working"));
        assert_eq!(pct, Some(50));
    }

    #[test]
    fn report_message_is_sticky_when_absent() {
        // LSP 规范：message 缺省表示沿用上一次，不能被清空。
        let mut t = ProgressTracker::default();
        t.apply(&token("a"), &begin("Indexing", Some(0)));
        t.apply(&token("a"), &report(Some("halfway"), Some(50)));
        t.apply(&token("a"), &report(None, Some(75)));

        let (_, msg, pct) = t.representative().unwrap();
        assert_eq!(msg.as_deref(), Some("halfway"), "message must persist");
        assert_eq!(pct, Some(75), "percentage should update");
    }

    #[test]
    fn percentage_is_sticky_when_absent() {
        let mut t = ProgressTracker::default();
        t.apply(&token("a"), &begin("Indexing", Some(40)));
        t.apply(&token("a"), &report(Some("more"), None));
        let (_, _, pct) = t.representative().unwrap();
        assert_eq!(pct, Some(40), "percentage should persist when not resent");
    }

    #[test]
    fn prefers_the_token_with_a_percentage() {
        // rust-analyzer 会同时开多个令牌；有百分比的那个信息量最大。
        let mut t = ProgressTracker::default();
        t.apply(&token("ra/Roots Scanned"), &begin("Roots Scanned", None));
        t.apply(&token("ra/Indexing"), &begin("Indexing", Some(7)));

        let (title, _, pct) = t.representative().unwrap();
        assert_eq!(title, "Indexing");
        assert_eq!(pct, Some(7));
    }

    #[test]
    fn representation_is_stable_across_calls() {
        // 多个令牌都没有百分比时，必须稳定选同一个（否则标签会闪烁）。
        let mut t = ProgressTracker::default();
        t.apply(&token("zzz"), &begin("Z", None));
        t.apply(&token("aaa"), &begin("A", None));
        t.apply(&token("mmm"), &begin("M", None));

        let first = t.representative().unwrap();
        for _ in 0..5 {
            assert_eq!(t.representative().unwrap(), first, "must not flicker");
        }
        // 按令牌名排序 → 选 "aaa"。
        assert_eq!(first.0, "A");
    }

    #[test]
    fn numeric_tokens_are_supported() {
        let mut t = ProgressTracker::default();
        t.apply(&ProgressToken::Number(7), &begin("Work", Some(10)));
        assert_eq!(t.representative().unwrap().0, "Work");
        t.apply(&ProgressToken::Number(7), &end());
        assert!(t.is_empty());
    }

    #[test]
    fn status_labels_are_readable() {
        assert_eq!(LspStatus::Starting.label(), "启动中");
        assert!(LspStatus::Starting.is_busy());

        let working = LspStatus::Working {
            title: "Indexing".into(),
            message: None,
            percentage: Some(42),
        };
        assert_eq!(working.label(), "Indexing 42%");
        assert!(working.is_busy());

        // 没有百分比时退回 message。
        let no_pct = LspStatus::Working {
            title: "Indexing".into(),
            message: Some("3/25 files".into()),
            percentage: None,
        };
        assert_eq!(no_pct.label(), "Indexing 3/25 files");

        // 就绪 + 诊断计数就是「实时状态」。
        assert_eq!(
            LspStatus::Ready {
                errors: 0,
                warnings: 0
            }
            .label(),
            "就绪"
        );
        assert_eq!(
            LspStatus::Ready {
                errors: 2,
                warnings: 1
            }
            .label(),
            "2 错误 1 警告"
        );
        assert_eq!(
            LspStatus::Ready {
                errors: 0,
                warnings: 3
            }
            .label(),
            "3 个警告"
        );
        assert!(
            !LspStatus::Ready {
                errors: 0,
                warnings: 0
            }
            .is_busy()
        );
        assert!(
            LspStatus::Failed {
                reason: "not found".into()
            }
            .label()
            .contains("not found")
        );
    }
}
