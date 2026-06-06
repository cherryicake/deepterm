use std::fs;
use std::path::PathBuf;

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

        // 确保配置目录存在
        if !config_dir.exists() {
            fs::create_dir_all(&config_dir).expect("Failed to create config directory");
        }

        Self { config_dir }
    }

    /// 获取配置目录路径
    fn get_config_dir() -> PathBuf {
        let home = dirs::home_dir().expect("Failed to get home directory");
        home.join(".config").join("deepterm")
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

    /// 保存工作区状态
    pub fn save_state(&self, state: &WorkspaceState) -> Result<(), Box<dyn std::error::Error>> {
        let file_path = self.get_state_file_path();
        let json = serde_json::to_string_pretty(state)?;
        fs::write(file_path, json)?;
        Ok(())
    }

    /// 加载工作区状态
    pub fn load_state(&self) -> Result<WorkspaceState, Box<dyn std::error::Error>> {
        let file_path = self.get_state_file_path();

        if !file_path.exists() {
            return Ok(WorkspaceState::default());
        }

        let json = fs::read_to_string(file_path)?;
        let state: WorkspaceState = serde_json::from_str(&json)?;
        Ok(state)
    }

    pub fn save_session(
        &self,
        session: &AppSessionState,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file_path = self.get_session_file_path();
        let json = serde_json::to_string_pretty(session)?;
        fs::write(file_path, json)?;
        Ok(())
    }

    pub fn load_session(&self) -> Result<Option<AppSessionState>, Box<dyn std::error::Error>> {
        let file_path = self.get_session_file_path();

        if !file_path.exists() {
            return Ok(None);
        }

        let json = fs::read_to_string(file_path)?;
        let session: AppSessionState = serde_json::from_str(&json)?;
        Ok(Some(session))
    }

    /// 保存模板
    pub fn save_templates(
        &self,
        templates: &[WorkspaceTemplate],
    ) -> Result<(), Box<dyn std::error::Error>> {
        let file_path = self.get_templates_file_path();
        let json = serde_json::to_string_pretty(templates)?;
        fs::write(file_path, json)?;
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
        fs::write(path, json)?;
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
