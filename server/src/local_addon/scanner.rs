use crate::local_addon::index::{IndexItem, LocalIndex};
use crate::local_addon::parser::parse_filename;
use crate::local_addon::resolver::resolve_imdb;
use reqwest::Client;
use tokio::task::{self, JoinHandle};
use walkdir::WalkDir;

pub(crate) fn scan_background(root: String, index: LocalIndex, client: Client) -> JoinHandle<()> {
    let index_ref = index;

    task::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<std::path::PathBuf>(128);

        task::spawn_blocking(move || {
            for entry in WalkDir::new(root)
                .into_iter()
                .filter_map(std::result::Result::ok)
            {
                if entry.file_type().is_file() && tx.blocking_send(entry.into_path()).is_err() {
                    break;
                }
            }
        });

        while let Some(path) = rx.recv().await {
            let path_str = path.to_string_lossy().to_string();

            // Handle .torrent files
            if path.extension().is_some_and(|ext| ext == "torrent") {
                if let Ok(bytes) = tokio::fs::read(&path).await {
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
                                    meta.imdb_id = imdb;
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
            } else if crate::archives::is_archive(&path) {
                // Archive handling
                if let Ok(reader) = crate::archives::get_archive_reader(&path).await
                    && let Ok(entries) = reader.list_files().await
                {
                    for entry in entries {
                        // Skip directories
                        if entry.is_dir {
                            continue;
                        }

                        let f_path = std::path::Path::new(&entry.path);
                        if let Some(mut meta) = parse_filename(f_path) {
                            // Resolve IMDB
                            if let Some(name) = &meta.name {
                                let imdb =
                                    resolve_imdb(&client, name, meta.year, &meta.type_).await;
                                meta.imdb_id = imdb;
                            }

                            let id = if let Some(ref imdb) = meta.imdb_id {
                                format!("local:{}", imdb)
                            } else {
                                String::new()
                            };

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
            } else if let Some(mut meta) = parse_filename(&path) {
                // Resolve IMDB (simple throttling might be needed in real env)
                if let Some(name) = &meta.name {
                    let imdb = resolve_imdb(&client, name, meta.year, &meta.type_).await;
                    meta.imdb_id = imdb;
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
        tracing::info!("Local Addon: Scan complete.");
    })
}
