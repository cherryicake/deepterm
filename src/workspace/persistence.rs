use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::session_state::AppSessionState;
use crate::workspace::template::{WorkspaceState, WorkspaceTemplate};

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
        Self::migrate_legacy_config_dir(&config_dir);

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

    /// 迁移旧配置目录（仅当新旧不同且旧存在、新不存在时）
    fn migrate_legacy_config_dir(new_dir: &Path) {
        let Some(home) = dirs::home_dir() else { return; };
        let legacy = home.join(".config").join("deepterm");
        if legacy == new_dir {
            return;
        }
        if !legacy.exists() || new_dir.exists() {
            return;
        }
        // 新目录不存在但旧目录存在，尝试迁移
        match fs::rename(&legacy, new_dir) {
            Ok(_) => log::info!("Migrated legacy config {} -> {}", legacy.display(), new_dir.display()),
            Err(e) => {
                // 跨文件系统 rename 失败则拷贝
                log::warn!("Failed to rename legacy config ({}), trying copy: {e}", legacy.display());
                if let Err(copy_err) = Self::copy_dir_all(&legacy, new_dir) {
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

    /// 获取状态文件路径
    fn get_state_file_path(&self) -> PathBuf {
        self.config_dir.join("workspace_state.json")
    }

    /// 获取模板文件路径
    fn get_templates_file_path(&self) -> PathBuf {
        self.config_dir.join("templates.json")
    }

    /// 兼容：尝试从旧路径加载并迁移
    fn legacy_session_path() -> Option<PathBuf> {
        let home = dirs::home_dir()?;
        let p = home.join(".config").join("deepterm").join("session_state.json");
        if p.exists() { Some(p) } else { None }
    }

    /// 保存工作区状态
    pub fn save_state(&self, state: &WorkspaceState) -> Result<(), Box<dyn std::error::Error>> {
        let file_path = self.get_state_file_path();
        let json = serde_json::to_string_pretty(state)?;
        Self::atomic_write(&file_path, json.as_bytes())?;
        Ok(())
    }

    /// 加载工作区状态
    pub fn load_state(&self) -> Result<WorkspaceState, Box<dyn std::error::Error>> {
        let file_path = self.get_state_file_path();

        if !file_path.exists() {
            return Ok(WorkspaceState::default());
        }

        let json = fs::read_to_string(&file_path)?;
        match serde_json::from_str(&json) {
            Ok(state) => Ok(state),
            Err(e) => {
                log::warn!("Failed to parse {}: {e}, trying .bak", file_path.display());
                let bak = file_path.with_extension("json.bak");
                if bak.exists() {
                    if let Ok(bak_json) = fs::read_to_string(&bak) {
                        if let Ok(state) = serde_json::from_str(&bak_json) {
                            log::info!("Recovered from backup {}", bak.display());
                            return Ok(state);
                        }
                    }
                }
                Err(Box::new(e))
            }
        }
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

    /// 保存模板
    pub fn save_templates(
        &self,
        templates: &[WorkspaceTemplate],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file_path = self.get_templates_file_path();
        let json = serde_json::to_string_pretty(templates)?;
        Self::atomic_write(&file_path, json.as_bytes())?;
        Ok(())
    }

    /// 加载模板
    pub fn load_templates(&self) -> Result<Vec<WorkspaceTemplate>, Box<dyn std::error::Error>> {
        let file_path = self.get_templates_file_path();

        if !file_path.exists() {
            return Ok(Vec::new());
        }

        let json = fs::read_to_string(file_path)?;
        let templates: Vec<WorkspaceTemplate> = serde_json::from_str(&json)?;
        Ok(templates)
    }

    /// 检查状态文件是否存在
    pub fn state_exists(&self) -> bool {
        self.get_state_file_path().exists()
    }

    /// 删除状态文件
    pub fn clear_state(&self) -> Result<(), Box<dyn std::error::Error>> {
        let file_path = self.get_state_file_path();
        if file_path.exists() {
            fs::remove_file(file_path)?;
        }
        Ok(())
    }

    /// 导出配置到指定路径
    pub fn export_config(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let state = self.load_state()?;
        let templates = self.load_templates()?;

        let export_data = ExportData {
            state,
            templates,
            export_version: "1.0".to_string(),
            export_date: chrono::Local::now().to_rfc3339(),
        };

        let json = serde_json::to_string_pretty(&export_data)?;
        Self::atomic_write(path, json.as_bytes())?;
        Ok(())
    }

    /// 从指定路径导入配置
    pub fn import_config(&self, path: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
        let json = fs::read_to_string(path)?;
        let export_data: ExportData = serde_json::from_str(&json)?;

        self.save_state(&export_data.state)?;
        self.save_templates(&export_data.templates)?;

        Ok(())
    }
}

impl Default for PersistenceManager {
    fn default() -> Self {
        Self::new()
    }
}

/// 导出数据结构
#[derive(Serialize, Deserialize)]
struct ExportData {
    state: WorkspaceState,
    templates: Vec<WorkspaceTemplate>,
    export_version: String,
    export_date: String,
}

/// 自动保存管理器
pub struct AutoSaveManager {
    persistence: PersistenceManager,
    last_save: Option<std::time::Instant>,
    save_interval: std::time::Duration,
}

impl AutoSaveManager {
    pub fn new() -> Self {
        Self {
            persistence: PersistenceManager::new(),
            last_save: None,
            save_interval: std::time::Duration::from_secs(30), // 30秒自动保存
        }
    }

    /// 检查是否需要自动保存
    pub fn should_auto_save(&self) -> bool {
        match self.last_save {
            None => true,
            Some(last) => last.elapsed() >= self.save_interval,
        }
    }

    /// 执行自动保存
    pub fn auto_save(&mut self, state: &WorkspaceState) -> Result<(), Box<dyn std::error::Error>> {
        self.persistence.save_state(state)?;
        self.last_save = Some(std::time::Instant::now());
        Ok(())
    }

    /// 设置自动保存间隔
    pub fn set_save_interval(&mut self, interval: std::time::Duration) {
        self.save_interval = interval;
    }
}

impl Default for AutoSaveManager {
    fn default() -> Self {
        Self::new()
    }
}
