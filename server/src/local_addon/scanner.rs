use walkdir::WalkDir;
use tokio::task;
use reqwest::Client;
use crate::local_addon::index::{LocalIndex, IndexItem};
use crate::local_addon::parser::parse_filename;
use crate::local_addon::resolver::resolve_imdb;

pub async fn scan_background(root: String, index: LocalIndex) {
    let root_path = root.clone();
    let index_ref = index.clone();
    
    task::spawn(async move {
        let client = Client::new();
        for entry in WalkDir::new(root_path).into_iter().filter_map(|e| e.ok()) {
            if entry.file_type().is_file() {
                let path = entry.path();
                let path_str = path.to_string_lossy().to_string();

                // Handle .torrent files
                if path.extension().map_or(false, |ext| ext == "torrent") {
                     if let Ok(bytes) = std::fs::read(path) {
                        // Use local torrent parser
                        if let Ok(torrent) = crate::local_addon::torrent::parse_torrent(&bytes) {
                            let info_hash = torrent.info_hash;
                            // Iterate files
                            for (idx, file) in torrent.files.iter().enumerate() {
                                let f_path = std::path::Path::new(&file.path);
                                if let Some(mut meta) = parse_filename(f_path) {
                                     // Resolve IMDB
                                     if let Some(name) = &meta.name {
                                        let type_ = meta.type_.clone();
                                        let year = meta.year;
                                        let imdb = resolve_imdb(&client, name, year, &type_).await;
                                        meta.imdb_id = imdb.clone();
                                     }

                                     let item = IndexItem {
                                         id: format!("bt:{}", info_hash),
                                         imdb_id: meta.imdb_id.clone(),
                                         metadata: meta,
                                         path: format!("torrent:{}", file.path),
                                         info_hash: Some(info_hash.clone()),
                                         file_idx: Some(idx),
                                     };
                                     index_ref.insert(item);
                                }
                            }
                        }
                     }
                } else if crate::archives::is_archive(path) {
                     // Archive handling
                     if let Ok(reader) = crate::archives::get_archive_reader(path).await {
                         if let Ok(entries) = reader.list_files().await {
                             for entry in entries {
                                 // Skip directories
                                 if entry.is_dir { continue; }
                                 
                                 let f_path = std::path::Path::new(&entry.path);
                                 if let Some(mut meta) = parse_filename(f_path) {
                                     // Resolve IMDB
                                     if let Some(name) = &meta.name {
                                         let imdb = resolve_imdb(&client, name, meta.year, &meta.type_).await;
                                         meta.imdb_id = imdb.clone();
                                     }

                                     // Construct ID: archive:{type}:{abs_path}:{internal_path}
                                     // But local_addon IDs are usually imdb_id based.
                                     // If we use local:ttXXXX, how do we know it's an archive stream?
                                     // The `path` field in IndexItem stores the location.
                                     
                                     // We can use the same ID logic as local files so they show up in catalogs.
                                     // The `path` string will identify it as an archive.
                                     
                                     let id = if let Some(ref imdb) = meta.imdb_id {
                                         format!("local:{}", imdb)
                                     } else {
                                         String::new()
                                     };
                                     
                                     // Encode path: archive:{archive_path}|{internal_path}
                                     // Using pipe separator to avoid colon confusion with drive letters? 
                                     // Or just standard: archive://{archive_path}?file={internal}
                                     // Let's use a custom prefix we can parse.
                                     let archive_marker = format!("archive:{}|{}", path_str, entry.path);

                                     let item = IndexItem {
                                         id,
                                         imdb_id: meta.imdb_id.clone(),
                                         metadata: meta,
                                         path: archive_marker,
                                         info_hash: None,
                                         file_idx: None,
                                     };
                                     index_ref.insert(item);
                                 }
                             }
                         }
                     }
                 } else if let Some(mut meta) = parse_filename(path) {
                    
                    // Resolve IMDB (simple throttling might be needed in real env)
                    if let Some(name) = &meta.name {
                        let imdb = resolve_imdb(&client, name, meta.year, &meta.type_).await;
                        meta.imdb_id = imdb.clone();
                    }
                    
                    // Allow insert() to handle local file ID generation (or set explicit local:imdb)
                    let id = if let Some(ref imdb) = meta.imdb_id {
                        format!("local:{}", imdb)
                    } else {
                         String::new() // fallback in insert
                    };

                    let item = IndexItem {
                        id,
                        imdb_id: meta.imdb_id.clone(),
                        metadata: meta,
                        path: path_str,
                        info_hash: None,
                        file_idx: None,
                    };
                    
                    index_ref.insert(item);
                }
            }
        }
        tracing::info!("Local Addon: Scan complete.");
    });
}
