//! 内建命令历史：记录用户提交过的完整命令行，下次输入同名命令时在补全弹窗中
//! 召回历史整行，免去逐段补全长参数（尤其是 `/scp` 的远端路径）。
//!
//! 持久化范式与 `theme.json` / `ssh.json` 一致：`dirs::config_dir()/deepterm/command_history.json`，
//! serde + 同步小 JSON 读写。记录只发生在回车提交时（低频），开销可忽略。

use parking_lot::Mutex;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 历史中保留的最大条目数，超出按 score 最低淘汰。
const MAX_ENTRIES: usize = 200;
/// 一次匹配最多返回的历史候选数。
const MAX_MATCHES: usize = 3;

/// 单条历史记录。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// 完整整行，如 `/scp alice:/etc/hosts ./`
    pub command_line: String,
    /// 首 token，如 `/scp`，用于命令名阶段过滤与统计
    pub command_name: String,
    /// 累计使用频次
    pub count: u32,
    /// 最近一次使用的 unix 秒
    pub last_used: i64,
}

impl HistoryEntry {
    /// 综合「频次 + 最近时间」的排序分。频次为主，最近时间做对数衰减加权。
    fn score(&self, now: i64) -> f64 {
        let age_secs = (now - self.last_used).max(0) as f64;
        // 1 天 ≈ 86400s；最近用过的加成更高，越旧衰减越多。
        let recency_boost = 1.0 / (1.0 + (age_secs / 86_400.0).ln_1p());
        self.count as f64 + recency_boost
    }
}

/// 命令历史集合。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CommandHistory {
    entries: Vec<HistoryEntry>,
}

impl CommandHistory {
    /// 从磁盘加载；文件不存在或解析失败时返回空历史。
    pub fn load() -> Self {
        let Some(path) = Self::config_path() else {
            return Self::default();
        };
        let Ok(content) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&content).unwrap_or_default()
    }

    /// 写回磁盘。失败仅记录日志，不阻塞输入。
    pub fn save(&self) {
        let Some(path) = Self::config_path() else {
            log::warn!("unable to resolve command history path");
            return;
        };
        if let Some(parent) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                log::warn!("failed to create command history dir: {err}");
                return;
            }
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => {
                if let Err(err) = std::fs::write(&path, json) {
                    log::warn!("failed to persist command history: {err}");
                }
            }
            Err(err) => log::warn!("failed to serialize command history: {err}"),
        }
    }

    /// 记录一条已提交的命令行。整行去重：命中则频次 +1、刷新时间；否则新增。
    /// 超出容量上限时淘汰 score 最低者。仅记录非空整行。
    pub fn record(&mut self, command_line: &str, now: i64) {
        let line = command_line.trim();
        if line.is_empty() {
            return;
        }
        let command_name = line.split_whitespace().next().unwrap_or(line).to_string();

        if let Some(existing) = self.entries.iter_mut().find(|e| e.command_line == line) {
            existing.count = existing.count.saturating_add(1);
            existing.last_used = now;
            existing.command_name = command_name;
        } else {
            self.entries.push(HistoryEntry {
                command_line: line.to_string(),
                command_name,
                count: 1,
                last_used: now,
            });
        }

        if self.entries.len() > MAX_ENTRIES {
            // 淘汰 score 最低的一条
            if let Some((idx, _)) = self.entries.iter().enumerate().min_by(|(_, a), (_, b)| {
                a.score(now)
                    .partial_cmp(&b.score(now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            }) {
                self.entries.remove(idx);
            }
        }
    }

    /// 按整行大小写不敏感子串匹配，按最近使用时间从新到旧返回 Top-N。
    ///
    /// `query` 为当前输入的整行（如 `/sc`、`/scp ali`）。空 query 返回最近使用的若干条。
    pub fn matches(&self, query: &str) -> Vec<HistoryEntry> {
        let q = query.trim().to_lowercase();
        let mut hits: Vec<&HistoryEntry> = self
            .entries
            .iter()
            .filter(|e| {
                if q.is_empty() {
                    return true;
                }
                // 整行子串匹配；同时排除「历史行与输入完全相同」的噪声
                let line_lc = e.command_line.to_lowercase();
                line_lc.contains(&q) && line_lc != q
            })
            .collect();

        hits.sort_by_key(|b| std::cmp::Reverse(b.last_used));
        hits.into_iter().take(MAX_MATCHES).cloned().collect()
    }

    fn config_path() -> Option<PathBuf> {
        let config_dir = dirs::config_dir()?;
        Some(config_dir.join("deepterm").join("command_history.json"))
    }
}

/// 全局历史单例。读写都很轻，直接用 Mutex 包裹。
fn global_history() -> &'static Mutex<CommandHistory> {
    use std::sync::OnceLock;
    static HISTORY: OnceLock<Mutex<CommandHistory>> = OnceLock::new();
    HISTORY.get_or_init(|| Mutex::new(CommandHistory::load()))
}

/// 当前 unix 秒。
fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// 记录一条已提交命令行并立即持久化（线程安全）。
pub fn record_command(command_line: &str) {
    let mut history = global_history().lock();
    history.record(command_line, now_secs());
    history.save();
}

/// 克隆全局历史的内存快照（线程安全）。供补全管理器注入使用，避免每次按键读盘。
pub fn snapshot() -> CommandHistory {
    global_history().lock().clone()
}

/// 当前 unix 秒（供管理器计算 recency 排序）。
pub fn current_unix_secs() -> i64 {
    now_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_dedups_and_bumps_count() {
        let mut h = CommandHistory::default();
        h.record("/scp a b", 100);
        h.record("/scp a b", 200);
        assert_eq!(h.entries.len(), 1);
        assert_eq!(h.entries[0].count, 2);
        assert_eq!(h.entries[0].last_used, 200);
        assert_eq!(h.entries[0].command_name, "/scp");
    }

    #[test]
    fn record_ignores_blank() {
        let mut h = CommandHistory::default();
        h.record("   ", 100);
        assert!(h.entries.is_empty());
    }

    #[test]
    fn matches_substring_case_insensitive() {
        let mut h = CommandHistory::default();
        h.record("/scp alice:/etc/hosts ./", 100);
        h.record("/open /var/log", 100);
        let hits = h.matches("ALI");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].command_line, "/scp alice:/etc/hosts ./");
    }

    #[test]
    fn matches_excludes_exact_equal_line() {
        let mut h = CommandHistory::default();
        h.record("/scp a b", 100);
        // 输入与历史完全一致时不应把自己作为候选
        assert!(h.matches("/scp a b").is_empty());
        // 但作为子串前缀仍可召回
        assert_eq!(h.matches("/scp a").len(), 1);
    }

    #[test]
    fn matches_ranks_more_frequent_first() {
        let mut h = CommandHistory::default();
        h.record("/scp rare:/x ./", 100);
        h.record("/scp common:/y ./", 100);
        h.record("/scp common:/y ./", 150);
        let hits = h.matches("/scp");
        assert_eq!(hits[0].command_line, "/scp common:/y ./");
    }

    #[test]
    fn record_evicts_when_over_capacity() {
        let mut h = CommandHistory::default();
        for i in 0..(MAX_ENTRIES + 10) {
            h.record(&format!("/scp host{i}:/x ./"), 100 + i as i64);
        }
        assert!(h.entries.len() <= MAX_ENTRIES);
    }
}
