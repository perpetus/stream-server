use crate::routes::system::ServerSettings;
use enginefs::EngineFS;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::local_addon::LocalIndex;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<EngineFS>,
    pub settings: Arc<RwLock<ServerSettings>>,
    pub settings_path: PathBuf,
    pub config_dir: PathBuf,
    pub local_index: LocalIndex,
    pub archive_cache: Arc<dashmap::DashMap<String, crate::archives::ArchiveSession>>,
    pub nzb_sessions: Arc<dashmap::DashMap<String, crate::archives::nzb::session::NzbSession>>,
}

impl AppState {
    pub fn new(engine: Arc<EngineFS>, settings: ServerSettings, config_dir: PathBuf) -> Self {
        let settings_path = config_dir.join("settings.json");

        Self {
            engine,
            settings: Arc::new(RwLock::new(settings)),
            settings_path,
            config_dir,
            local_index: LocalIndex::new(),
            archive_cache: Arc::new(dashmap::DashMap::new()),
            nzb_sessions: Arc::new(dashmap::DashMap::new()),
        }
    }

    pub async fn save_settings(&self) -> anyhow::Result<()> {
        let settings = self.settings.read().await;
        let json = serde_json::to_string_pretty(&*settings)?;

        // Ensure parent directory exists
        if let Some(parent) = self.settings_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        tokio::fs::write(&self.settings_path, json).await?;
        tracing::info!("Settings saved to {:?}", self.settings_path);
        Ok(())
    }

    pub fn load_settings(
        config_dir: &std::path::Path,
        defaults: &ServerSettings,
    ) -> ServerSettings {
        let settings_path = config_dir.join("settings.json");

        if settings_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&settings_path) {
                if let Ok(mut settings) = serde_json::from_str::<ServerSettings>(&content) {
                    tracing::info!("Loaded settings from {:?}", settings_path);
                    // Ensure the cache_root in the loaded settings matches what we expect from runtime?
                    // Or do we respect the file?
                    // User might have customized it in the file.
                    // If it's the default value (empty or old default), maybe update it?
                    // For now, let's respect the file, but if missing/empty, use our runtime defaults.
                    if settings.cache_root.is_empty() {
                        settings.cache_root = defaults.cache_root.clone();
                    }
                    return settings;
                }
            }
        }

        tracing::info!("Using default settings");
        defaults.clone()
    }
}
