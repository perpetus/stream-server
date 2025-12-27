use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::tracker_prober::TrackerProber;

const DEFAULT_TRACKERS_URL: &str =
    "https://raw.githubusercontent.com/ngosang/trackerslist/master/trackers_best.txt";
const REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60); // 24 hours
const REFRESH_CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60); // Check every hour

/// Trait for tracker persistence (implemented by AppState or similar)
/// This allows enginefs to persist trackers without depending on server crate
pub trait TrackerStorage: Send + Sync {
    /// Get cached trackers from settings
    fn get_cached_trackers(&self) -> Vec<String>;
    /// Get Unix timestamp when trackers were last updated
    fn get_last_updated(&self) -> i64;
    /// Get the URL to fetch tracker list from
    fn get_source_url(&self) -> String;
    /// Save trackers and timestamp to persistent storage
    fn save_trackers(&self, trackers: Vec<String>, timestamp: i64);
}

#[derive(Clone)]
pub struct TrackerManager {
    trackers: Arc<RwLock<Vec<String>>>,
    storage: Option<Arc<dyn TrackerStorage>>,
}

impl Default for TrackerManager {
    fn default() -> Self {
        Self::new()
    }
}

impl TrackerManager {
    /// Create a new TrackerManager without persistence (legacy behavior)
    pub fn new() -> Self {
        let instance = Self {
            trackers: Arc::new(RwLock::new(Vec::new())),
            storage: None,
        };

        // Initial fetch and periodic refresh
        let manager = instance.clone();
        tokio::spawn(async move {
            // Initial fetch
            if let Err(e) = manager
                .refresh_trackers_internal(DEFAULT_TRACKERS_URL)
                .await
            {
                warn!(error = %e, "Failed to fetch initial trackers");
            }

            let mut interval = tokio::time::interval(REFRESH_CHECK_INTERVAL);
            loop {
                interval.tick().await;
                if let Err(e) = manager
                    .refresh_trackers_internal(DEFAULT_TRACKERS_URL)
                    .await
                {
                    warn!(error = %e, "Failed to refresh trackers");
                }
            }
        });

        instance
    }

    /// Create a new TrackerManager with persistence support
    pub fn new_with_storage(storage: Arc<dyn TrackerStorage>) -> Self {
        let instance = Self {
            trackers: Arc::new(RwLock::new(Vec::new())),
            storage: Some(storage.clone()),
        };

        // Initial load from cache + periodic refresh
        let manager = instance.clone();
        tokio::spawn(async move {
            // Try to load from cache first
            manager.load_from_cache().await;

            // Check if refresh is needed
            if let Err(e) = manager.refresh_if_needed().await {
                warn!(error = %e, "Failed initial tracker refresh");
            }

            // Periodic check for refresh
            let mut interval = tokio::time::interval(REFRESH_CHECK_INTERVAL);
            loop {
                interval.tick().await;
                if let Err(e) = manager.refresh_if_needed().await {
                    warn!(error = %e, "Failed to refresh trackers");
                }
            }
        });

        instance
    }

    /// Load trackers from cache (settings)
    async fn load_from_cache(&self) {
        if let Some(ref storage) = self.storage {
            let cached = storage.get_cached_trackers();
            if !cached.is_empty() {
                info!(count = cached.len(), "Loaded cached trackers from settings");
                let mut guard = self.trackers.write().await;
                *guard = cached;
            }
        }
    }

    /// Check if trackers need refresh (older than 24 hours) and refresh if needed
    async fn refresh_if_needed(&self) -> anyhow::Result<()> {
        let Some(ref storage) = self.storage else {
            // No storage, just fetch
            return self.refresh_trackers_internal(DEFAULT_TRACKERS_URL).await;
        };

        let last_updated = storage.get_last_updated();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let age_secs = now - last_updated;

        // Check if trackers are fresh (less than 24 hours old)
        if age_secs < REFRESH_INTERVAL.as_secs() as i64 {
            // Ensure we have trackers loaded
            let guard = self.trackers.read().await;
            if !guard.is_empty() {
                debug!(
                    age_hours = age_secs / 3600,
                    "Trackers still fresh, skipping refresh"
                );
                return Ok(());
            }
            drop(guard);

            // Trackers empty, try to load from cache
            let cached = storage.get_cached_trackers();
            if !cached.is_empty() {
                let mut guard = self.trackers.write().await;
                *guard = cached;
                return Ok(());
            }
        }

        // Need to refresh - fetch, rank, and persist
        let source_url = storage.get_source_url();
        info!(url = %source_url, "Fetching and ranking trackers");

        let raw_trackers = self.fetch_trackers(&source_url).await?;
        if raw_trackers.is_empty() {
            warn!("Fetched empty tracker list");
            return Ok(());
        }

        debug!(
            count = raw_trackers.len(),
            "Fetched raw trackers, ranking by RTT"
        );

        // Rank trackers by RTT
        let ranked = TrackerProber::rank_trackers(raw_trackers).await;

        // Filter out unreachable trackers (those with max duration from failed probes)
        // Keep top 20 fastest trackers
        let top_trackers: Vec<String> = ranked.into_iter().take(20).collect();

        info!(count = top_trackers.len(), "Ranked and cached top trackers");

        // Persist to storage
        storage.save_trackers(top_trackers.clone(), now);

        // Update in-memory cache
        let mut guard = self.trackers.write().await;
        *guard = top_trackers;

        Ok(())
    }

    /// Internal refresh without persistence (legacy behavior)
    async fn refresh_trackers_internal(&self, url: &str) -> anyhow::Result<()> {
        let raw_trackers = self.fetch_trackers(url).await?;
        if !raw_trackers.is_empty() {
            debug!(count = raw_trackers.len(), "Fetched trackers");
            let mut guard = self.trackers.write().await;
            *guard = raw_trackers;
        }
        Ok(())
    }

    /// Fetch trackers from URL
    async fn fetch_trackers(&self, url: &str) -> anyhow::Result<Vec<String>> {
        let response = reqwest::get(url).await?;
        let text = response.text().await?;

        let mut trackers = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() {
                trackers.push(line.to_string());
            }
        }

        Ok(trackers)
    }

    /// Get the current list of trackers
    pub async fn get_trackers(&self) -> Vec<String> {
        let guard = self.trackers.read().await;
        if guard.is_empty() {
            // Fallback if empty (shouldn't happen if fetch works, but good to have)
            Vec::new()
        } else {
            guard.clone()
        }
    }

    /// Force refresh trackers now (useful for manual refresh via API)
    pub async fn force_refresh(&self) -> anyhow::Result<()> {
        if let Some(ref storage) = self.storage {
            // Reset last updated to force refresh
            storage.save_trackers(Vec::new(), 0);
        }
        self.refresh_if_needed().await
    }
}
