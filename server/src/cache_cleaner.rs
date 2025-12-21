use crate::state::AppState;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

pub fn start(state: Arc<AppState>) {
    tokio::spawn(async move {
        debug!("Cache cleaner started");

        // Channel for file system events
        let (tx, mut rx) = mpsc::channel::<()>(100);

        // Setup Watcher
        // We use a sync watcher bridge to async channel
        let tx_clone = tx.clone();
        let mut watcher = match RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        // Filter interesting events
                        if matches!(
                            event.kind,
                            notify::EventKind::Create(_)
                                | notify::EventKind::Modify(_)
                                | notify::EventKind::Remove(_)
                        ) {
                            let _ = tx_clone.blocking_send(());
                        }
                    }
                    Err(e) => error!("Watch error: {:?}", e),
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                error!("Failed to create watcher: {}", e);
                return;
            }
        };

        // Initial watch
        // We might need to retry if directory doesn't exist yet
        let download_dir = state.engine.download_dir.clone();
        if let Err(e) = watcher.watch(&download_dir, RecursiveMode::Recursive) {
            warn!("Failed to watch download dir: {}", e);
            // We will try to re-watch inside the loop if needed (omitted for brevity, relying on fallback poll)
        }

        // Timer for fallback polling (1 hour)
        let mut poll_interval = tokio::time::interval(Duration::from_secs(3600));

        // State for debouncing
        let debounce_duration = Duration::from_secs(60); // Wait 60s after last activity to clean
        let mut active_cleaning_timer = Box::pin(tokio::time::sleep(Duration::MAX)); // Inactive initially

        loop {
            tokio::select! {
                // 1. Fallback / Periodic Poll
                _ = poll_interval.tick() => {
                    debug!("Periodic cache clean trigger");
                    if let Err(e) = clean_cache(&state).await {
                        error!("Cache cleaner error: {}", e);
                    }
                    // Re-ensure watch if needed
                    if let Err(e) = watcher.watch(&state.engine.download_dir, RecursiveMode::Recursive) {
                        debug!("Retry watch: {}", e);
                    }
                }

                // 2. File System Event
                Some(_) = rx.recv() => {
                    // Reset debounce timer
                    active_cleaning_timer = Box::pin(tokio::time::sleep(debounce_duration));
                }

                // 3. Debounce Timer Fired
                _ = &mut active_cleaning_timer => {
                    debug!("Debounced cache clean trigger");
                    if let Err(e) = clean_cache(&state).await {
                        error!("Cache cleaner error: {}", e);
                    }
                    // Reset timer to infinite
                    active_cleaning_timer = Box::pin(tokio::time::sleep(Duration::MAX));
                }
            }
        }
    });
}

async fn clean_cache(state: &Arc<AppState>) -> anyhow::Result<()> {
    let settings = state.settings.read().await;
    let limit = settings.cache_size as u64;
    drop(settings); // Release lock

    let download_dir = &state.engine.download_dir;
    if !download_dir.exists() {
        return Ok(());
    }

    // 1. Identify protected files matching current active engines
    let engines = state.engine.get_all_statistics().await;
    let mut protected_paths = HashSet::new();

    for (_, stats) in engines {
        if !stats.files.is_empty() {
            for file in stats.files {
                let path = download_dir.join(&file.path);
                protected_paths.insert(path);
            }
        } else {
            let path = download_dir.join(&stats.name);
            protected_paths.insert(path);
        }
    }

    // 2. Scan and Evict immediately based on age (30 days)
    let thirty_days = Duration::from_secs(30 * 24 * 60 * 60);
    let now = std::time::SystemTime::now();

    let mut files = Vec::new();
    let mut total_size = 0u64;

    let mut entries = walkdir::WalkDir::new(download_dir).into_iter();

    loop {
        match entries.next() {
            Some(Ok(entry)) => {
                if entry.file_type().is_file() {
                    let path = entry.path().to_path_buf();
                    // Is protected?
                    let is_protected = protected_paths.contains(&path)
                        || protected_paths.iter().any(|p| path.starts_with(p));

                    if let Ok(metadata) = entry.metadata() {
                        let size = metadata.len();

                        if is_protected {
                            total_size += size;
                            continue;
                        }

                        if let Ok(modified) = metadata.modified() {
                            // Check AGE
                            let age = now
                                .duration_since(modified)
                                .unwrap_or(Duration::from_secs(0));
                            if age > thirty_days {
                                info!("File older than 30 days, deleting: {:?}", path);
                                if let Err(e) = tokio::fs::remove_file(&path).await {
                                    error!("Failed to delete file {:?}: {}", path, e);
                                    // Count it in total size since we failed to delete?
                                    // Or ignore? Let's count it to be safe for cache limit.
                                    total_size += size;
                                } else {
                                    // Successfully deleted, do not add to total_size
                                    // Try to clean empty parent dir
                                    if let Some(parent) = path.parent() {
                                        if parent != download_dir {
                                            let _ = tokio::fs::remove_dir(parent).await;
                                        }
                                    }
                                }
                            } else {
                                // Keep for potentially size-based eviction
                                total_size += size;
                                files.push((path, size, modified));
                            }
                        } else {
                            // Could not read time, keep it but count size
                            total_size += size;
                            files.push((path, size, std::time::SystemTime::UNIX_EPOCH));
                        }
                    }
                }
            }
            Some(Err(e)) => {
                debug!("Error walking directory: {}", e);
            }
            None => break,
        }
    }

    // 3. Size-based Eviction
    if limit > 0 && total_size > limit {
        info!(
            "Cache size {} exceeds limit {}. Cleaning up...",
            total_size, limit
        );

        // Sort by modification time (oldest first)
        files.sort_by(|a, b| a.2.cmp(&b.2));

        let mut deleted_count = 0;
        let mut freed_space = 0;

        for (path, size, _) in files {
            if total_size <= limit {
                break;
            }

            debug!("Deleting old file (size limit): {:?}", path);
            if let Err(e) = tokio::fs::remove_file(&path).await {
                error!("Failed to delete file {:?}: {}", path, e);
            } else {
                total_size = total_size.saturating_sub(size);
                freed_space += size;
                deleted_count += 1;

                if let Some(parent) = path.parent() {
                    if parent != download_dir {
                        let _ = tokio::fs::remove_dir(parent).await;
                    }
                }
            }
        }

        info!(
            "Cleaned up {} files, freed {} bytes. New size: {}",
            deleted_count, freed_space, total_size
        );
    }

    Ok(())
}
