use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, warn};

const TRACKERS_URL: &str =
    "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt";
const REFRESH_INTERVAL: Duration = Duration::from_secs(60 * 60); // 1 hour

#[derive(Clone)]
pub struct TrackerManager {
    trackers: Arc<RwLock<Vec<String>>>,
}

impl TrackerManager {
    pub fn new() -> Self {
        let instance = Self {
            trackers: Arc::new(RwLock::new(Vec::new())),
        };

        // Initial fetch and periodic refresh
        let manager = instance.clone();
        tokio::spawn(async move {
            // Initial fetch
            if let Err(e) = manager.refresh_trackers().await {
                warn!(error = %e, "Failed to fetch initial trackers");
            }

            let mut interval = tokio::time::interval(REFRESH_INTERVAL);
            loop {
                interval.tick().await;
                if let Err(e) = manager.refresh_trackers().await {
                    warn!(error = %e, "Failed to refresh trackers");
                }
            }
        });

        instance
    }

    async fn refresh_trackers(&self) -> anyhow::Result<()> {
        let response = reqwest::get(TRACKERS_URL).await?;
        let text = response.text().await?;

        let mut new_trackers = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() {
                new_trackers.push(line.to_string());
            }
        }

        if !new_trackers.is_empty() {
            debug!(count = new_trackers.len(), "Fetched trackers");
            let mut guard = self.trackers.write().await;
            *guard = new_trackers;
        }

        Ok(())
    }

    pub async fn get_trackers(&self) -> Vec<String> {
        let guard = self.trackers.read().await;
        if guard.is_empty() {
            // Fallback if empty (shouldn't happen if fetch works, but good to have)
            // returning empty list, engine will fallback or use provided
            Vec::new()
        } else {
            guard.clone()
        }
    }
}
