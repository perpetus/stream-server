use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, RwLock};
use crate::local_addon::parser::VideoMetadata;

#[derive(Debug, Clone)]
pub struct IndexItem {
    pub id: String, // local:ttXXXXXX or file path hash if unknown
    pub imdb_id: Option<String>,
    pub metadata: VideoMetadata,
    pub path: String,
    pub info_hash: Option<String>,
    pub file_idx: Option<usize>,
}

#[derive(Clone)]
pub struct LocalIndex {
    // Map IMDB ID -> List of Files (VideoMetadata + Path)
    // Helps aggregating episodes for a series
    pub items: Arc<RwLock<HashMap<String, Vec<IndexItem>>>>,
    // Quick lookup for Path -> ID (to avoid re-indexing)
    pub path_map: Arc<RwLock<HashMap<String, String>>>,
}

impl LocalIndex {
    pub fn new() -> Self {
        Self {
            items: Arc::new(RwLock::new(HashMap::new())),
            path_map: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn insert(&self, mut item: IndexItem) {
        let mut items = self.items.write().unwrap();
        let key = if !item.id.is_empty() {
            item.id.clone()
        } else if let Some(ref imdb) = item.imdb_id {
            format!("local:{}", imdb)
        } else {
            // Fallback for non-identified items: Hash the path to be deterministic!
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            item.path.hash(&mut hasher);
            format!("local:{}", hasher.finish())
        };
        
        item.id = key.clone();
        
        items.entry(key.clone()).or_insert_with(Vec::new).push(item.clone());
        
        let mut paths = self.path_map.write().unwrap();
        paths.insert(item.path.clone(), key);
    }
    

}

use std::hash::Hasher;
