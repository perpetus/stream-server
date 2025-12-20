use crate::routes::system::ServerSettings;
use enginefs::EngineFS;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Clone)]
pub struct AppState {
    pub engine: Arc<EngineFS>,
    pub settings: Arc<RwLock<ServerSettings>>,
    pub settings_path: PathBuf,
}

impl AppState {
    pub fn new(engine: Arc<EngineFS>, settings: ServerSettings) -> Self {
        let cache_root = &settings.cache_root;
        let settings_path = PathBuf::from(cache_root).join("settings.json");

        Self {
            engine,
            settings: Arc::new(RwLock::new(settings)),
            settings_path,
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

    pub fn load_settings(cache_root: &str) -> ServerSettings {
        let settings_path = PathBuf::from(cache_root).join("settings.json");

        if settings_path.exists() {
            if let Ok(content) = std::fs::read_to_string(&settings_path) {
                if let Ok(settings) = serde_json::from_str::<ServerSettings>(&content) {
                    tracing::info!("Loaded settings from {:?}", settings_path);
                    return settings;
                }
            }
        }

        tracing::info!("Using default settings");
        ServerSettings::default()
    }
}
