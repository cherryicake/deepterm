use std::fs;
use std::path::{Path, PathBuf};

use crate::workspace::session_state::AppSessionState;

/// 持久化管理器
pub struct PersistenceManager {
    config_dir: PathBuf,
}

impl PersistenceManager {
    /// 创建新的持久化管理器
    pub fn new() -> Self {
        let config_dir = Self::get_config_dir();

        // 确保配置目录存在；失败仅日志，不崩溃
        if !config_dir.exists() {
            if let Err(e) = fs::create_dir_all(&config_dir) {
                log::error!("Failed to create config directory {}: {e}", config_dir.display());
            }
        }

        // 自动迁移旧路径 ~/.config/deepterm -> dirs::config_dir()/deepterm
        if let Some(home) = dirs::home_dir() {
            let legacy = home.join(".config").join("deepterm");
            Self::migrate_legacy_dir(&legacy, &config_dir);
        }

        Self { config_dir }
    }

    /// 测试专用：指定配置目录，跳过系统路径探测与真实旧路径迁移。
    #[cfg(test)]
    fn with_config_dir(config_dir: PathBuf) -> Self {
        Self { config_dir }
    }

    /// 获取配置目录路径：优先使用系统标准 config_dir，失败回退到 home/.config
    fn get_config_dir() -> PathBuf {
        if let Some(dir) = dirs::config_dir() {
            return dir.join("deepterm");
        }
        if let Some(home) = dirs::home_dir() {
            log::warn!("dirs::config_dir() unavailable, falling back to ~/.config/deepterm");
            return home.join(".config").join("deepterm");
        }
        // 极端情况：无 home，退化到临时目录，避免 panic
        let fallback = std::env::temp_dir().join("deepterm");
        log::error!("No config/home directory found, using temp fallback: {}", fallback.display());
        fallback
    }

    /// 迁移旧配置目录（仅当新旧不同且旧存在、新不存在时）。
    /// 抽为纯函数以便测试：调用方负责提供真实的旧/新路径。
    fn migrate_legacy_dir(legacy: &Path, new_dir: &Path) {
        if legacy == new_dir {
            return;
        }
        if !legacy.exists() || new_dir.exists() {
            return;
        }
        // 新目录不存在但旧目录存在，尝试迁移
        match fs::rename(legacy, new_dir) {
            Ok(_) => log::info!("Migrated legacy config {} -> {}", legacy.display(), new_dir.display()),
            Err(e) => {
                // 跨文件系统 rename 失败则拷贝
                log::warn!("Failed to rename legacy config ({}), trying copy: {e}", legacy.display());
                if let Err(copy_err) = Self::copy_dir_all(legacy, new_dir) {
                    log::error!("Failed to migrate legacy config: {copy_err}");
                } else {
                    log::info!("Copied legacy config {} -> {}", legacy.display(), new_dir.display());
                }
            }
        }
    }

    fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dst)?;
        for entry in fs::read_dir(src)? {
            let entry = entry?;
            let ty = entry.file_type()?;
            let dst_path = dst.join(entry.file_name());
            if ty.is_dir() {
                Self::copy_dir_all(&entry.path(), &dst_path)?;
            } else {
                fs::copy(entry.path(), dst_path)?;
            }
        }
        Ok(())
    }

    /// 原子写入：先写临时文件再 rename，避免崩溃产生截断 JSON
    fn atomic_write(path: &Path, content: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // 使用 pid + 随机避免并发冲突
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        fs::write(&tmp, content)?;
        // 保留旧文件备份（仅 session/state）
        if path.exists() {
            let bak = path.with_extension(format!("{}.bak", path.extension().and_then(|s| s.to_str()).unwrap_or("json")));
            let _ = fs::copy(path, &bak);
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    fn get_session_file_path(&self) -> PathBuf {
        self.config_dir.join("session_state.json")
    }

    /// 兼容：尝试从旧路径加载并迁移
    fn legacy_session_path() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        let p = home.join(".config").join("deepterm").join("session_state.json");
        if p.exists() { Some(p) } else { None }
    }

    pub fn save_session(
        &self,
        session: &AppSessionState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file_path = self.get_session_file_path();
        let json = serde_json::to_string_pretty(session)?;
        Self::atomic_write(&file_path, json.as_bytes())?;
        Ok(())
    }

    pub fn load_session(&self) -> Result<Option<AppSessionState>, Box<dyn std::error::Error>> {
        let file_path = self.get_session_file_path();

        if !file_path.exists() {
            // 兼容旧路径
            if let Some(legacy) = Self::legacy_session_path() {
                if legacy != file_path {
                    log::info!("Found legacy session at {}, migrating", legacy.display());
                    match fs::read_to_string(&legacy) {
                        Ok(json) => match serde_json::from_str::<AppSessionState>(&json) {
                            Ok(session) => {
                                // 迁移到新路径
                                if let Ok(new_json) = serde_json::to_string_pretty(&session) {
                                    let _ = Self::atomic_write(&file_path, new_json.as_bytes());
                                }
                                return Ok(Some(session));
                            }
                            Err(e) => log::warn!("Failed to parse legacy session: {e}"),
                        },
                        Err(e) => log::warn!("Failed to read legacy session: {e}"),
                    }
                }
            }
            return Ok(None);
        }

        let json = fs::read_to_string(&file_path)?;
        match serde_json::from_str(&json) {
            Ok(session) => Ok(Some(session)),
            Err(e) => {
                log::warn!("Failed to parse {}: {e}, trying .bak", file_path.display());
                let bak = file_path.with_extension("json.bak");
                if bak.exists() {
                    if let Ok(bak_json) = fs::read_to_string(&bak) {
                        if let Ok(session) = serde_json::from_str(&bak_json) {
                            log::info!("Recovered session from backup {}", bak.display());
                            return Ok(Some(session));
                        }
                    }
                }
                Err(Box::new(e))
            }
        }
    }
}

impl Default for PersistenceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::workspace::session_state::WorkspaceSessionState;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deepterm-persist-test-{}-{}-{}",
            tag,
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn sample_session() -> AppSessionState {
        let mut session = AppSessionState::default();
        session.groups.push(crate::workspace::session_state::WorkspaceGroupSessionState {
            id: "g1".into(),
            label: Some("测试组".into()),
            collapsed: false,
            workspace: Some(WorkspaceSessionState {
                workspace_id: "ws-1".into(),
                active_tab_id: "tab-0".into(),
                next_tab_id: 1,
                next_panel_id: 1,
                local_terminal_count: 1,
                tabs: vec![],
            }),
        });
        session
    }

    /// 清理临时目录（测试结束调用；失败静默）
    fn cleanup(dir: &Path) {
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn save_and_load_session_roundtrip() {
        let dir = temp_dir("roundtrip");
        {
            let mgr = PersistenceManager::with_config_dir(dir.clone());
            mgr.save_session(&sample_session()).expect("save");
            let loaded = mgr.load_session().expect("load").expect("some session");
            assert_eq!(loaded.groups.len(), 1);
            assert_eq!(loaded.groups[0].id, "g1");
            assert_eq!(loaded.version, crate::workspace::session_state::SESSION_STATE_VERSION);
        }
        cleanup(&dir);
    }

    #[test]
    fn corrupted_main_file_recovers_from_bak() {
        let dir = temp_dir("bak-recovery");
        {
            let mgr = PersistenceManager::with_config_dir(dir.clone());
            // 第一次写入生成主文件，第二次写入生成 .bak（内容不同以便区分）
            let mut v1 = sample_session();
            v1.active_group_id = "v1".into();
            mgr.save_session(&v1).expect("save v1");
            let mut v2 = sample_session();
            v2.active_group_id = "v2".into();
            mgr.save_session(&v2).expect("save v2");

            // 模拟断电截断：主文件写坏
            let main = dir.join("session_state.json");
            fs::write(&main, "{\"version\":2,\"acti").expect("corrupt");

            // 加载应回退到 .bak（即 v1）
            let loaded = mgr.load_session().expect("load falls back to bak").expect("some");
            assert_eq!(loaded.active_group_id, "v1");
        }
        cleanup(&dir);
    }

    #[test]
    fn atomic_write_creates_parent_dirs() {
        let dir = temp_dir("parents");
        let nested = dir.join("a").join("b").join("session_state.json");
        PersistenceManager::atomic_write(&nested, b"{}").expect("write into nested parents");
        assert!(nested.exists());
        cleanup(&dir);
    }

    #[test]
    fn legacy_dir_migrated_when_new_absent() {
        let base = temp_dir("legacy-dir");
        let legacy = base.join("legacy-config");
        let new_dir = base.join("new-config");
        fs::create_dir_all(&legacy).expect("legacy dir");

        // 旧目录里放一份会话与 ssh 配置
        let session = sample_session();
        fs::write(
            legacy.join("session_state.json"),
            serde_json::to_string_pretty(&session).unwrap(),
        )
        .unwrap();
        fs::write(legacy.join("ssh.json"), "{\"saved_connections\":[]}").unwrap();

        PersistenceManager::migrate_legacy_dir(&legacy, &new_dir);

        // 整目录迁移：新目录存在、旧目录消失
        assert!(new_dir.exists());
        assert!(!legacy.exists());
        assert!(new_dir.join("ssh.json").exists());
        let mgr = PersistenceManager::with_config_dir(new_dir.clone());
        let loaded = mgr.load_session().expect("load migrated session").expect("some");
        assert_eq!(loaded.groups.len(), 1);
        cleanup(&base);
    }

    #[test]
    fn migration_skipped_when_new_dir_exists() {
        let base = temp_dir("no-overwrite");
        let legacy = base.join("legacy");
        let new_dir = base.join("new");
        fs::create_dir_all(&legacy).unwrap();
        fs::create_dir_all(&new_dir).unwrap();
        fs::write(legacy.join("marker.txt"), "old").unwrap();
        fs::write(new_dir.join("marker.txt"), "new").unwrap();

        PersistenceManager::migrate_legacy_dir(&legacy, &new_dir);

        // 新目录已存在：绝不覆盖，两边原样保留
        assert!(legacy.exists());
        assert_eq!(
            fs::read_to_string(new_dir.join("marker.txt")).unwrap(),
            "new"
        );
        cleanup(&base);
    }
}
