use crate::engine::Engine;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::debug;

pub mod backend;
pub mod engine;
pub mod error;
pub mod files;
pub mod hls;
pub mod hwaccel;
pub mod metadata_cache;
pub mod metadata_pins;
pub mod piece_cache;
pub mod piece_waiter;
pub mod subtitles;
pub mod tracker_prober;
pub mod trackers;

// Re-export TrackerStorage and errors for use by server crate
pub use error::{EngineError, Result as EngineResult};
pub use trackers::TrackerStorage;

#[cfg(feature = "libtorrent")]
use crate::backend::libtorrent::LibtorrentBackend;

use crate::backend::{
    BackendMemoryDiagnostics, HotFilePriorityPlan, TorrentBackend, TorrentFilePriorityPlan,
    TorrentHandle, TorrentSource,
};

const INACTIVE_TORRENT_REMOVE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
const INACTIVE_TORRENT_PAUSE_GRACE: Duration = Duration::from_secs(15);
const HLS_PLAYBACK_LEASE_TTL: Duration = Duration::from_secs(300);
const LIBTORRENT_HLS_PLAYBACK_LEASE_TTL: Duration = Duration::from_secs(15);

static START_TIME: OnceLock<Instant> = OnceLock::new();

type EngineRegistry<H> = Arc<RwLock<HashMap<String, Arc<Engine<H>>>>>;

pub fn now_secs() -> u64 {
    START_TIME.get_or_init(Instant::now).elapsed().as_secs()
}

fn hls_playback_lease_ttl_secs() -> u64 {
    HLS_PLAYBACK_LEASE_TTL.as_secs()
}

fn playback_lease_is_active(lease: &PlaybackLease, now: u64) -> bool {
    lease.expires_at_secs > now
}

const DEFAULT_TRACKERS: &[&str] = &[
    "udp://tracker.opentrackr.org:1337/announce",
    "udp://9.rarbg.com:2810/announce",
    "udp://tracker.openbittorrent.com:80/announce",
    "http://tracker.openbittorrent.com:80/announce",
    "udp://opentracker.i2p.rocks:6969/announce",
    "udp://open.stealth.si:80/announce",
    "udp://tracker.torrent.eu.org:451/announce",
    "udp://tracker.tiny-vps.com:6969/announce",
    "udp://tracker.moeking.me:6969/announce",
    "udp://ipv4.tracker.harry.lu:80/announce",
];

pub struct BackendEngineFS<B: TorrentBackend> {
    pub backend: Arc<B>,
    engines: EngineRegistry<B::Handle>,
    tracker_manager: Arc<crate::trackers::TrackerManager>,
    pub cache_dir: std::path::PathBuf,
    pub download_dir: std::path::PathBuf,
    pub stream_state: Arc<RwLock<StreamState>>,
    priority_generation: Arc<AtomicU64>,
    /// When false, torrents are paused once their download completes.
    seeding_enabled: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Default)]
pub struct StreamState {
    /// Track active streams per info_hash for legacy compatibility
    pub active_streams: HashMap<String, usize>,
    /// Track active requests per specific streamed file so cleanup does not race probe retries.
    pub active_file_streams: HashMap<(String, usize), usize>,
    /// Tracks the most recently active streamed file for legacy diagnostics.
    pub active_file: Option<(String, usize)>,
    /// HLS playback is made of short segment reads. A lease keeps the file wanted
    /// while the player is buffered and no response body is currently open.
    pub active_playback_leases: HashMap<(String, usize), PlaybackLease>,
    /// For multi-file torrents, only the latest requested file is allowed to be
    /// wanted at a time. Single-file torrents bypass this selector.
    pub active_multifile_files: HashMap<String, MultiFileActiveSelection>,
}

#[derive(Debug, Clone)]
pub struct PlaybackLease {
    pub last_seen_secs: u64,
    pub expires_at_secs: u64,
}

#[derive(Debug, Clone)]
pub struct MultiFileActiveSelection {
    pub file_idx: usize,
    pub generation: u64,
    pub source: &'static str,
    pub last_seen_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackMode {
    Streaming,
    KeepDownloading,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActiveFileStreamSnapshot {
    pub info_hash: String,
    pub file_idx: usize,
    pub count: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActiveFileSnapshot {
    pub info_hash: String,
    pub file_idx: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivePlaybackLeaseSnapshot {
    pub info_hash: String,
    pub file_idx: usize,
    pub last_seen_secs: u64,
    pub expires_in_secs: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MultiFileActiveSelectionSnapshot {
    pub info_hash: String,
    pub file_idx: usize,
    pub generation: u64,
    pub source: String,
    pub last_seen_secs: u64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct StreamActivitySnapshot {
    pub uptime_secs: u64,
    pub engine_count: usize,
    pub engine_active_streams: usize,
    pub active_file_priority_generation: u64,
    pub active_streams: HashMap<String, usize>,
    pub active_file_streams: Vec<ActiveFileStreamSnapshot>,
    pub active_file: Option<ActiveFileSnapshot>,
    pub active_playback_leases: Vec<ActivePlaybackLeaseSnapshot>,
    pub active_multifile_selections: Vec<MultiFileActiveSelectionSnapshot>,
    pub idle_paused_torrents: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EngineDiagnosticsSnapshot {
    pub uptime_secs: u64,
    pub streams: StreamActivitySnapshot,
    pub memory: BackendMemoryDiagnostics,
}

#[cfg(feature = "libtorrent")]
pub type EngineFS = BackendEngineFS<LibtorrentBackend>;

impl<B: TorrentBackend + 'static> BackendEngineFS<B> {
    pub fn new_with_backend(
        backend: B,
        restored_handles: HashMap<String, B::Handle>,
        cache_dir: std::path::PathBuf,
        download_dir: std::path::PathBuf,
    ) -> Self {
        Self::new_with_backend_and_storage(backend, restored_handles, cache_dir, download_dir, None)
    }

    pub fn new_with_backend_and_storage(
        backend: B,
        restored_handles: HashMap<String, B::Handle>,
        cache_dir: std::path::PathBuf,
        download_dir: std::path::PathBuf,
        tracker_storage: Option<Arc<dyn crate::trackers::TrackerStorage>>,
    ) -> Self {
        let mut engines_map = HashMap::new();
        for (hash, handle) in restored_handles {
            engines_map.insert(
                hash.clone(),
                Arc::new(Engine::new_with_handle(handle, &hash)),
            );
        }

        let engines = Arc::new(RwLock::new(engines_map));

        // Create tracker manager with or without storage
        let tracker_manager = match tracker_storage {
            Some(storage) => Arc::new(crate::trackers::TrackerManager::new_with_storage(storage)),
            None => Arc::new(crate::trackers::TrackerManager::new()),
        };

        let efs = Self {
            backend: Arc::new(backend),
            engines: Arc::clone(&engines),
            tracker_manager,
            cache_dir,
            download_dir,
            stream_state: Arc::new(RwLock::new(StreamState::default())),
            priority_generation: Arc::new(AtomicU64::new(0)),
            seeding_enabled: Arc::new(AtomicBool::new(true)),
        };

        let engines_clone = engines;
        let backend_clone = Arc::clone(&efs.backend);
        let stream_state_clone = Arc::clone(&efs.stream_state);
        let seeding_flag = Arc::clone(&efs.seeding_enabled);
        tokio::spawn(async move {
            loop {
                // Run fairly frequently so seeding stops promptly after the
                // user disables it; torrent removal is still gated by the much
                // longer inactivity timeout below, so this only changes how
                // quickly the seeding-disabled pause reacts.
                tokio::time::sleep(Duration::from_secs(15)).await;
                let mut to_remove = Vec::new();
                let now = now_secs();

                let expired_leases = {
                    let mut state = stream_state_clone.write().await;
                    let expired = state
                        .active_playback_leases
                        .iter()
                        .filter(|(_, lease)| !playback_lease_is_active(lease, now))
                        .map(|(key, _)| key.clone())
                        .collect::<Vec<_>>();
                    for key in &expired {
                        state.active_playback_leases.remove(key);
                    }
                    expired
                };
                for (info_hash, file_idx) in expired_leases {
                    tracing::info!(
                        info_hash = %info_hash,
                        file_idx,
                        ttl_secs = hls_playback_lease_ttl_secs(),
                        "HLS playback lease expired"
                    );

                    let (still_active, active_selection) = {
                        let mut state = stream_state_clone.write().await;
                        let active = state
                            .active_file_streams
                            .get(&(info_hash.clone(), file_idx))
                            .copied()
                            .unwrap_or(0)
                            > 0;
                        if active {
                            (true, None)
                        } else {
                            let sel = match state.active_multifile_files.get(&info_hash).cloned() {
                                Some(selection) if selection.file_idx == file_idx => {
                                    state.active_multifile_files.remove(&info_hash);
                                    tracing::info!(
                                        info_hash = %info_hash,
                                        file_idx,
                                        generation = selection.generation,
                                        reason = "hls-lease-expired",
                                        "multifile_active_file_cleared"
                                    );
                                    None
                                }
                                Some(selection) => Some(selection),
                                None => None,
                            };
                            if let Some((ref h, idx)) = state.active_file
                                && h == &info_hash
                                && idx == file_idx
                            {
                                state.active_file = sel
                                    .as_ref()
                                    .map(|selection| (info_hash.clone(), selection.file_idx));
                            }
                            (false, sel)
                        }
                    };
                    if still_active {
                        continue;
                    }

                    let engine = {
                        let engines = engines_clone.read().await;
                        engines.get(&info_hash).cloned()
                    };
                    if let Some(engine) = engine {
                        if engine.handle.manages_playback_lifecycle() {
                            // The libtorrent coordinator expires its own
                            // generation-scoped HLS lease and performs the
                            // acknowledged pause. Shared delayed cleanup must
                            // not race it.
                            continue;
                        }
                        let reconciled = Self::reconcile_multifile_engine(
                            Arc::clone(&engine),
                            active_selection
                                .as_ref()
                                .map(|selection| selection.file_idx),
                            None,
                            active_selection
                                .as_ref()
                                .map_or(0, |selection| selection.generation),
                            "hls-lease-expired",
                        )
                        .await;
                        if reconciled {
                            continue;
                        }

                        if let Err(err) = engine.handle.clear_file_streaming(file_idx).await {
                            tracing::warn!(
                                info_hash = %info_hash,
                                file_idx,
                                error = %err,
                                "Failed to clear file priorities after HLS playback lease expired"
                            );
                        } else {
                            tracing::info!(
                                info_hash = %info_hash,
                                file_idx,
                                "Cleared file priorities after HLS playback lease expired"
                            );
                        }
                    }
                }

                {
                    let read = engines_clone.read().await;
                    for (hash, engine) in read.iter() {
                        let engine_active_streams = engine
                            .active_streams
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let last = engine
                            .last_accessed
                            .load(std::sync::atomic::Ordering::SeqCst);
                        let age_secs = now.saturating_sub(last);
                        if age_secs <= INACTIVE_TORRENT_REMOVE_TIMEOUT.as_secs() {
                            continue;
                        }

                        let (
                            active_stream_count,
                            active_file_stream_count,
                            active_playback_lease_count,
                            active_file_matches,
                            active_multifile_matches,
                        ) = {
                            let state = stream_state_clone.read().await;
                            let s_count = state.active_streams.get(hash).copied().unwrap_or(0);
                            let fs_count = state
                                .active_file_streams
                                .iter()
                                .filter(|((stream_hash, _), _)| stream_hash == hash)
                                .map(|(_, count)| *count)
                                .sum::<usize>();
                            let pl_count = state
                                .active_playback_leases
                                .iter()
                                .filter(|((stream_hash, _), lease)| {
                                    stream_hash == hash && playback_lease_is_active(lease, now)
                                })
                                .count();
                            let f_matches = state
                                .active_file
                                .as_ref()
                                .is_some_and(|(stream_hash, _)| stream_hash == hash);
                            let mf_matches = state.active_multifile_files.contains_key(hash);
                            (s_count, fs_count, pl_count, f_matches, mf_matches)
                        };

                        let skip_reason = if engine_active_streams > 0 {
                            Some("engine_active_streams")
                        } else if active_stream_count > 0 {
                            Some("active_streams")
                        } else if active_file_stream_count > 0 {
                            Some("active_file_streams")
                        } else if active_playback_lease_count > 0 {
                            Some("active_playback_leases")
                        } else if active_file_matches {
                            Some("active_file")
                        } else if active_multifile_matches {
                            Some("active_multifile_file")
                        } else {
                            None
                        };

                        if let Some(skip_reason) = skip_reason {
                            tracing::debug!(
                                info_hash = %hash,
                                age_secs,
                                engine_active_streams,
                                active_stream_count,
                                active_file_stream_count,
                                active_playback_lease_count,
                                active_multifile_matches,
                                removed = false,
                                skip_reason,
                                "Skipping inactive-engine cleanup"
                            );
                        } else {
                            tracing::debug!(
                                info_hash = %hash,
                                age_secs,
                                engine_active_streams,
                                active_stream_count,
                                active_file_stream_count,
                                active_playback_lease_count,
                                active_multifile_matches,
                                removed = true,
                                "Scheduling inactive-engine cleanup"
                            );
                            to_remove.push(hash.clone());
                        }
                    }
                }

                if !to_remove.is_empty() {
                    let mut write = engines_clone.write().await;
                    for hash in &to_remove {
                        debug!(info_hash = %hash, "Auto-removing inactive engine");
                        write.remove(hash);
                    }
                    drop(write);

                    // Actually stop the torrents in the backend session
                    for hash in to_remove {
                        if let Err(e) = backend_clone.remove_torrent(&hash).await {
                            tracing::warn!(
                                info_hash = %hash,
                                error = %e,
                                removed = false,
                                "Failed to remove inactive torrent from backend"
                            );
                        } else {
                            tracing::info!(
                                info_hash = %hash,
                                removed = true,
                                "Removed inactive torrent from backend"
                            );
                        }
                    }
                }

                // Stop all torrent activity when seeding is disabled and no
                // playback is active. A later playback request resumes the
                // torrent before making its requested file wanted.
                if !seeding_flag.load(Ordering::Relaxed) {
                    let read = engines_clone.read().await;
                    for (hash, engine) in read.iter() {
                        if engine.handle.manages_playback_lifecycle() {
                            continue;
                        }
                        let (hash_active, file_active, playback_active, multifile_active) = {
                            let state = stream_state_clone.read().await;
                            let h_active = state.active_streams.get(hash).copied().unwrap_or(0) > 0;
                            let f_active = state
                                .active_file_streams
                                .iter()
                                .any(|((stream_hash, _), count)| stream_hash == hash && *count > 0);
                            let p_active = state.active_playback_leases.iter().any(
                                |((stream_hash, _), lease)| {
                                    stream_hash == hash && playback_lease_is_active(lease, now)
                                },
                            );
                            let m_active = state.active_multifile_files.contains_key(hash);
                            (h_active, f_active, p_active, m_active)
                        };
                        let reader_active = engine.active_streams.load(Ordering::SeqCst) > 0;
                        if hash_active
                            || file_active
                            || playback_active
                            || multifile_active
                            || reader_active
                        {
                            continue;
                        }

                        // A magnet that is still fetching its info dictionary
                        // must remain connected to the swarm. Inactive engines
                        // are removed by the separate cleanup policy.
                        if !engine.handle.stats().await.has_metadata {
                            continue;
                        }

                        if engine.idle_paused.swap(true, Ordering::Relaxed) {
                            continue;
                        }

                        tracing::info!(
                            info_hash = %hash,
                            "torrent_paused_idle"
                        );
                        if let Err(e) = engine.handle.pause_torrent().await {
                            tracing::warn!(
                                info_hash = %hash,
                                error = %e,
                                "Failed to pause idle torrent"
                            );
                            engine.idle_paused.store(false, Ordering::Relaxed);
                        }
                    }
                }
            }
        });

        efs
    }

    pub async fn add_torrent(
        &self,
        source: TorrentSource,
        extra_trackers: Option<Vec<String>>,
    ) -> Result<Arc<Engine<B::Handle>>> {
        // Start with default trackers
        let mut trackers: Vec<String> = DEFAULT_TRACKERS
            .iter()
            .map(std::string::ToString::to_string)
            .collect();

        // Add cached trackers from tracker manager (already ranked by RTT)
        let cached_trackers = self.tracker_manager.get_trackers().await;
        trackers.extend(cached_trackers);

        // Add any extra trackers provided
        if let Some(extra) = extra_trackers {
            trackers.extend(extra);
        }
        trackers.sort();
        trackers.dedup();

        debug!(count = trackers.len(), "Adding torrent with trackers");

        let handle = self.backend.add_torrent(source, trackers).await?;
        let info_hash = handle.info_hash();

        let mut engines = self.engines.write().await;
        if let Some(engine) = engines.get(&info_hash) {
            engine.touch();
            return Ok(Arc::clone(engine));
        }

        let engine = Arc::new(Engine::new_with_handle(handle, &info_hash));
        engines.insert(info_hash.clone(), Arc::clone(&engine));
        Ok(engine)
    }

    pub async fn get_engine(&self, info_hash: &str) -> Option<Arc<Engine<B::Handle>>> {
        let engines = self.engines.read().await;
        let engine = engines.get(&info_hash.to_lowercase()).cloned();
        if let Some(engine) = &engine {
            engine.touch();
        }
        engine
    }

    pub async fn get_or_add_engine(&self, info_hash: &str) -> Result<Arc<Engine<B::Handle>>> {
        if let Some(engine) = self.get_engine(info_hash).await {
            return Ok(engine);
        }
        let magnet = format!("magnet:?xt=urn:btih:{info_hash}");
        self.add_torrent(TorrentSource::Url(magnet), None).await
    }

    pub async fn remove_engine(&self, info_hash: &str) {
        let mut engines = self.engines.write().await;
        engines.remove(&info_hash.to_lowercase());
    }

    pub async fn get_all_statistics(&self) -> HashMap<String, crate::backend::EngineStats> {
        let engines = self.engines.read().await;
        let mut stats = HashMap::new();
        for (hash, engine) in engines.iter() {
            stats.insert(hash.clone(), engine.get_statistics().await);
        }
        stats
    }

    pub async fn list_engines(&self) -> Vec<String> {
        let engines = self.engines.read().await;
        engines.keys().cloned().collect()
    }

    pub async fn stream_activity_snapshot(&self) -> StreamActivitySnapshot {
        let engines = self.engines.read().await;
        let engine_count = engines.len();
        let engine_active_streams = engines
            .values()
            .map(|engine| {
                engine
                    .active_streams
                    .load(std::sync::atomic::Ordering::SeqCst)
            })
            .sum();
        let idle_paused_torrents = engines
            .iter()
            .filter(|(_, engine)| engine.idle_paused.load(Ordering::Relaxed))
            .map(|(hash, _)| hash.clone())
            .collect();
        drop(engines);

        let now = now_secs();
        let state = self.stream_state.read().await;
        let active_streams = state.active_streams.clone();
        let active_file_streams = state
            .active_file_streams
            .iter()
            .map(|((info_hash, file_idx), count)| ActiveFileStreamSnapshot {
                info_hash: info_hash.clone(),
                file_idx: *file_idx,
                count: *count,
            })
            .collect();
        let active_file =
            state
                .active_file
                .as_ref()
                .map(|(info_hash, file_idx)| ActiveFileSnapshot {
                    info_hash: info_hash.clone(),
                    file_idx: *file_idx,
                });
        let active_playback_leases = state
            .active_playback_leases
            .iter()
            .filter(|(_, lease)| playback_lease_is_active(lease, now))
            .map(
                |((info_hash, file_idx), lease)| ActivePlaybackLeaseSnapshot {
                    info_hash: info_hash.clone(),
                    file_idx: *file_idx,
                    last_seen_secs: lease.last_seen_secs,
                    expires_in_secs: lease.expires_at_secs.saturating_sub(now),
                },
            )
            .collect();
        let active_multifile_selections = state
            .active_multifile_files
            .iter()
            .map(|(info_hash, selection)| MultiFileActiveSelectionSnapshot {
                info_hash: info_hash.clone(),
                file_idx: selection.file_idx,
                generation: selection.generation,
                source: selection.source.to_string(),
                last_seen_secs: selection.last_seen_secs,
            })
            .collect();
        drop(state);

        StreamActivitySnapshot {
            uptime_secs: now,
            engine_count,
            engine_active_streams,
            active_file_priority_generation: self.priority_generation.load(Ordering::Relaxed),
            active_streams,
            active_file_streams,
            active_file,
            active_playback_leases,
            active_multifile_selections,
            idle_paused_torrents,
        }
    }

    pub async fn diagnostics_snapshot(&self) -> EngineDiagnosticsSnapshot {
        let streams = self.stream_activity_snapshot().await;
        let memory = self.backend.memory_diagnostics().await;

        EngineDiagnosticsSnapshot {
            uptime_secs: now_secs(),
            streams,
            memory,
        }
    }

    /// Called when a stream starts for a torrent file.
    /// Several torrent files may be active at once; cleanup is per file stream.
    pub async fn on_stream_start(&self, info_hash: &str, file_idx: usize) {
        let info_hash = info_hash.to_lowercase();
        let native_lifecycle = self
            .get_engine(&info_hash)
            .await
            .is_some_and(|engine| engine.handle.manages_playback_lifecycle());
        if native_lifecycle {
            if let Some(engine) = self.get_engine(&info_hash).await {
                engine.touch();
            }
            self.stream_state.write().await.active_file = Some((info_hash.clone(), file_idx));
        } else {
            self.activate_file(&info_hash, file_idx, PlaybackMode::Streaming, "stream")
                .await;
        }

        // Also update legacy active_streams counter
        {
            let mut state = self.stream_state.write().await;
            *state.active_streams.entry(info_hash.clone()).or_insert(0) += 1;
            *state
                .active_file_streams
                .entry((info_hash.clone(), file_idx))
                .or_insert(0) += 1;
        }

        tracing::debug!(
            "Stream started for {} file_idx={} (shared mode)",
            info_hash,
            file_idx
        );
    }

    async fn activate_file(
        &self,
        info_hash: &str,
        file_idx: usize,
        mode: PlaybackMode,
        source: &'static str,
    ) {
        let mut is_multifile = false;
        if let Some(engine) = self.get_engine(info_hash).await {
            engine.touch();
            if engine.handle.manages_playback_lifecycle() {
                self.stream_state.write().await.active_file =
                    Some((info_hash.to_string(), file_idx));
                return;
            }
            is_multifile = engine.handle.file_count().await > 1;

            // Any new playback activity must resume a torrent that was paused
            // by the idle seeding-disabled policy.
            if let Err(err) = engine.handle.resume_torrent().await {
                tracing::warn!(
                    info_hash = %info_hash,
                    file_idx,
                    error = %err,
                    source,
                    "Failed to resume torrent for active stream"
                );
            } else if engine.idle_paused.swap(false, Ordering::Relaxed) {
                tracing::info!(
                    info_hash = %info_hash,
                    file_idx,
                    source,
                    "torrent_resumed_for_stream"
                );
            }

            if mode == PlaybackMode::KeepDownloading
                && !is_multifile
                && let Err(err) = engine.handle.keep_file_downloading(file_idx).await
            {
                tracing::warn!(
                    info_hash = %info_hash,
                    file_idx,
                    error = %err,
                    source,
                    "Failed to keep HLS playback file downloading"
                );
            }
        }

        self.stream_state.write().await.active_file = Some((info_hash.to_string(), file_idx));

        if is_multifile {
            self.activate_multifile_file(info_hash, file_idx, None, source)
                .await;
        }
    }

    pub async fn activate_multifile_file_for_playback(
        &self,
        info_hash: &str,
        file_idx: usize,
        hot_file: Option<HotFilePriorityPlan>,
        source: &'static str,
    ) {
        let info_hash = info_hash.to_lowercase();
        if self
            .get_engine(&info_hash)
            .await
            .is_some_and(|engine| engine.handle.manages_playback_lifecycle())
        {
            return;
        }
        self.activate_multifile_file(&info_hash, file_idx, hot_file, source)
            .await;
    }

    async fn activate_multifile_file(
        &self,
        info_hash: &str,
        file_idx: usize,
        hot_file: Option<HotFilePriorityPlan>,
        source: &'static str,
    ) {
        let Some(engine) = self.get_engine(info_hash).await else {
            return;
        };

        if engine.handle.file_count().await <= 1 {
            return;
        }

        let generation = self.priority_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let now = now_secs();
        let previous_file_idx = {
            let mut state = self.stream_state.write().await;
            let prev = state
                .active_multifile_files
                .insert(
                    info_hash.to_string(),
                    MultiFileActiveSelection {
                        file_idx,
                        generation,
                        source,
                        last_seen_secs: now,
                    },
                )
                .map(|selection| selection.file_idx);

            state
                .active_playback_leases
                .retain(|(hash, idx), _| hash.as_str() != info_hash || *idx == file_idx);
            state
                .active_file_streams
                .retain(|(hash, idx), _| hash.as_str() != info_hash || *idx == file_idx);
            state.active_file = Some((info_hash.to_string(), file_idx));
            prev
        };

        engine.touch();
        if let Err(err) = engine.handle.resume_torrent().await {
            tracing::warn!(
                info_hash = %info_hash,
                file_idx,
                generation,
                error = %err,
                source,
                "Failed to resume torrent for multi-file active file"
            );
        } else {
            engine.idle_paused.store(false, Ordering::Relaxed);
        }

        Self::reconcile_multifile_engine(engine, Some(file_idx), hot_file, generation, source)
            .await;

        tracing::debug!(
            info_hash = %info_hash,
            file_idx,
            previous_file_idx,
            generation,
            source,
            "multifile_active_file_selected"
        );
    }

    async fn reconcile_multifile_engine(
        engine: Arc<Engine<B::Handle>>,
        active_file: Option<usize>,
        hot_file: Option<HotFilePriorityPlan>,
        generation: u64,
        reason: &'static str,
    ) -> bool {
        if engine.handle.manages_playback_lifecycle() {
            return true;
        }
        if engine.handle.file_count().await <= 1 {
            return false;
        }

        if let Err(err) = engine
            .handle
            .reconcile_file_priorities(TorrentFilePriorityPlan {
                active_file,
                hot_file,
                generation,
                reason,
            })
            .await
        {
            tracing::warn!(
                info_hash = %engine.info_hash,
                generation,
                reason,
                error = %err,
                "Failed to reconcile multi-file priorities"
            );
        }

        true
    }

    /// Refresh an HLS playback lease. HLS segment reads are short-lived, so this
    /// keeps the requested file wanted while the player is buffered.
    pub async fn refresh_hls_playback(
        &self,
        info_hash: &str,
        file_idx: usize,
        source: &'static str,
    ) {
        let info_hash = info_hash.to_lowercase();
        let now = now_secs();
        let engine = self.get_engine(&info_hash).await;
        let native_lifecycle = engine
            .as_ref()
            .is_some_and(|engine| engine.handle.manages_playback_lifecycle());
        let ttl = if native_lifecycle {
            LIBTORRENT_HLS_PLAYBACK_LEASE_TTL
        } else {
            HLS_PLAYBACK_LEASE_TTL
        };
        {
            let mut state = self.stream_state.write().await;
            state.active_playback_leases.insert(
                (info_hash.clone(), file_idx),
                PlaybackLease {
                    last_seen_secs: now,
                    expires_at_secs: now.saturating_add(ttl.as_secs()),
                },
            );
        }

        if let Some(engine) = engine {
            engine.touch();
            if native_lifecycle {
                self.stream_state.write().await.active_file = Some((info_hash.clone(), file_idx));
                if let Err(error) = engine.handle.refresh_hls_activity(file_idx, source).await {
                    tracing::warn!(
                        info_hash = %info_hash,
                        file_idx,
                        source,
                        %error,
                        "Failed to refresh libtorrent HLS playback"
                    );
                }
            } else {
                self.activate_file(&info_hash, file_idx, PlaybackMode::KeepDownloading, source)
                    .await;
            }
        }

        tracing::debug!(
            info_hash = %info_hash,
            file_idx,
            source,
            ttl_secs = ttl.as_secs(),
            "HLS playback lease refreshed"
        );
    }

    /// Refresh a lease only if playback is already known to be active. This is
    /// used by stats.json so a progress poll cannot create a new download.
    pub async fn refresh_existing_hls_playback(
        &self,
        info_hash: &str,
        file_idx: usize,
        source: &'static str,
    ) -> bool {
        let info_hash = info_hash.to_lowercase();
        let now = now_secs();
        let engine = self.get_engine(&info_hash).await;
        let native_lifecycle = engine
            .as_ref()
            .is_some_and(|engine| engine.handle.manages_playback_lifecycle());
        let ttl = if native_lifecycle {
            LIBTORRENT_HLS_PLAYBACK_LEASE_TTL
        } else {
            HLS_PLAYBACK_LEASE_TTL
        };
        let refreshed = {
            let mut state = self.stream_state.write().await;
            let key = (info_hash.clone(), file_idx);
            match state.active_playback_leases.get_mut(&key) {
                Some(lease) if playback_lease_is_active(lease, now) => {
                    lease.last_seen_secs = now;
                    lease.expires_at_secs = now.saturating_add(ttl.as_secs());
                    true
                }
                Some(_) => {
                    state.active_playback_leases.remove(&key);
                    false
                }
                None => false,
            }
        };

        if refreshed {
            if let Some(engine) = engine {
                if native_lifecycle {
                    engine.touch();
                    if let Err(error) = engine.handle.refresh_hls_activity(file_idx, source).await {
                        tracing::warn!(
                            info_hash = %info_hash,
                            file_idx,
                            source,
                            %error,
                            "Failed to refresh existing libtorrent HLS playback"
                        );
                    }
                } else {
                    self.activate_file(&info_hash, file_idx, PlaybackMode::KeepDownloading, source)
                        .await;
                }
            }
            tracing::debug!(
                info_hash = %info_hash,
                file_idx,
                source,
                "Existing HLS playback lease refreshed"
            );
        }

        refreshed
    }

    /// End an HLS playback lease immediately when the client sends an explicit
    /// destroy signal. Silent page closes are handled by lease expiry.
    pub async fn end_hls_playback(&self, info_hash: &str, file_idx: usize, reason: &'static str) {
        let info_hash = info_hash.to_lowercase();
        let removed = {
            let mut state = self.stream_state.write().await;
            state
                .active_playback_leases
                .remove(&(info_hash.clone(), file_idx))
                .is_some()
        };

        let engine = self.get_engine(&info_hash).await;
        let native_lifecycle = engine
            .as_ref()
            .is_some_and(|engine| engine.handle.manages_playback_lifecycle());

        if removed || native_lifecycle {
            tracing::info!(
                info_hash = %info_hash,
                file_idx,
                reason,
                "HLS playback lease ended"
            );
            if let Some(engine) = engine
                && native_lifecycle
            {
                if let Err(error) = engine.handle.end_hls_activity(file_idx, reason).await {
                    tracing::warn!(
                        info_hash = %info_hash,
                        file_idx,
                        reason,
                        %error,
                        "Failed to end libtorrent HLS playback"
                    );
                }
                return;
            }
            self.schedule_file_cleanup(info_hash.clone(), file_idx)
                .await;
            self.schedule_torrent_pause(info_hash);
        }
    }

    /// Called when a stream ends for a torrent file
    pub async fn on_stream_end(&self, info_hash: &str, file_idx: usize) {
        let info_hash = info_hash.to_lowercase();

        let (hash_streams_remaining, file_streams_remaining, remaining) = {
            let mut state = self.stream_state.write().await;
            let h_rem = if let Some(count) = state.active_streams.get_mut(&info_hash) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.active_streams.remove(&info_hash);
                    0
                } else {
                    *count
                }
            } else {
                0
            };

            let key = (info_hash.clone(), file_idx);
            let f_rem = if let Some(count) = state.active_file_streams.get_mut(&key) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    state.active_file_streams.remove(&key);
                    0
                } else {
                    *count
                }
            } else {
                0
            };

            let total = state.active_streams.values().sum::<usize>();
            (h_rem, f_rem, total)
        };

        let native_lifecycle = if let Some(engine) = self.get_engine(&info_hash).await {
            // Reset idle age so removal happens after the stream becomes
            // inactive, not after the stream originally started.
            engine.touch();
            engine.handle.manages_playback_lifecycle()
        } else {
            false
        };

        if !native_lifecycle && hash_streams_remaining == 0 && file_streams_remaining == 0 {
            self.schedule_torrent_pause(info_hash.clone());
        }

        if !native_lifecycle && file_streams_remaining == 0 {
            self.schedule_file_cleanup(info_hash.clone(), file_idx)
                .await;
        }

        tracing::debug!(
            "Stream ended for {} file_idx={}, total active streams: {}, file streams remaining: {}",
            info_hash,
            file_idx,
            remaining,
            file_streams_remaining
        );
    }

    /// Promptly pause an idle torrent shortly after the last stream on it ends
    /// when seeding is disabled. A later playback request resumes it before
    /// selecting the requested file.
    fn schedule_torrent_pause(&self, info_hash: String) {
        // Dropping a Tokio JoinHandle detaches the task instead of cancelling it.
        drop(self.schedule_torrent_pause_after(info_hash, INACTIVE_TORRENT_PAUSE_GRACE));
    }

    fn schedule_torrent_pause_after(
        &self,
        info_hash: String,
        delay: Duration,
    ) -> tokio::task::JoinHandle<()> {
        let engines = Arc::clone(&self.engines);
        let stream_state = Arc::clone(&self.stream_state);
        let seeding_enabled = Arc::clone(&self.seeding_enabled);

        tokio::spawn(async move {
            tokio::time::sleep(delay).await;

            if seeding_enabled.load(Ordering::Relaxed) {
                return;
            }

            let (hash_active, file_active, playback_active, multifile_active) = {
                let now = now_secs();
                let state = stream_state.read().await;
                let h_active = state.active_streams.get(&info_hash).copied().unwrap_or(0) > 0;
                let f_active = state
                    .active_file_streams
                    .iter()
                    .any(|((hash, _), count)| hash == &info_hash && *count > 0);
                let p_active = state
                    .active_playback_leases
                    .iter()
                    .any(|((hash, _), lease)| {
                        hash == &info_hash && playback_lease_is_active(lease, now)
                    });
                let m_active = state.active_multifile_files.contains_key(&info_hash);
                (h_active, f_active, p_active, m_active)
            };
            if hash_active || file_active || playback_active || multifile_active {
                tracing::debug!(
                    info_hash = %info_hash,
                    hash_active,
                    file_active,
                    playback_active,
                    multifile_active,
                    "Skipping idle pause because stream activity resumed"
                );
                return;
            }

            let engine = {
                let engines = engines.read().await;
                engines.get(&info_hash).cloned()
            };
            if let Some(engine) = engine {
                let reader_active = engine.active_streams.load(Ordering::SeqCst) > 0;
                if reader_active {
                    tracing::debug!(
                        info_hash = %info_hash,
                        reader_active,
                        "Skipping idle pause because a file or metadata reader is active"
                    );
                    return;
                }
                if !engine.handle.stats().await.has_metadata {
                    tracing::debug!(
                        info_hash = %info_hash,
                        "Skipping idle pause while torrent metadata is unresolved"
                    );
                    return;
                }
                engine.touch();
                if engine.idle_paused.swap(true, Ordering::Relaxed) {
                    return;
                }
                tracing::info!(
                    info_hash = %info_hash,
                    "torrent_paused_idle"
                );
                if let Err(err) = engine.handle.pause_torrent().await {
                    tracing::warn!(
                        info_hash = %info_hash,
                        error = %err,
                        "Failed to pause inactive torrent after grace period"
                    );
                    engine.idle_paused.store(false, Ordering::Relaxed);
                }
            }
        })
    }

    async fn schedule_file_cleanup(&self, info_hash: String, file_idx: usize) {
        drop(
            self.schedule_file_cleanup_after(info_hash, file_idx, Duration::from_secs(5))
                .await,
        );
    }

    async fn schedule_file_cleanup_after(
        &self,
        info_hash: String,
        file_idx: usize,
        delay: Duration,
    ) -> Option<tokio::task::JoinHandle<()>> {
        if self
            .get_engine(&info_hash)
            .await
            .is_some_and(|engine| engine.handle.manages_playback_lifecycle())
        {
            return None;
        }
        let engines = Arc::clone(&self.engines);
        let stream_state = Arc::clone(&self.stream_state);
        let scheduled_generation = {
            let state = self.stream_state.read().await;
            state
                .active_multifile_files
                .get(&info_hash)
                .filter(|selection| selection.file_idx == file_idx)
                .map(|selection| selection.generation)
        };

        Some(tokio::spawn(async move {
            tokio::time::sleep(delay).await;

            let key = (info_hash.clone(), file_idx);
            let active_selection = {
                let now = now_secs();
                let mut state = stream_state.write().await;
                let still_active = state.active_file_streams.get(&key).copied().unwrap_or(0) > 0;
                if still_active {
                    tracing::debug!(
                        "Skipping delayed cleanup for {} idx={} because a new stream started",
                        info_hash,
                        file_idx
                    );
                    return;
                }
                let playback_active = state
                    .active_playback_leases
                    .get(&key)
                    .is_some_and(|lease| playback_lease_is_active(lease, now));
                if playback_active {
                    tracing::info!(
                        info_hash = %info_hash,
                        file_idx,
                        "Skipping delayed cleanup because HLS playback lease is active"
                    );
                    return;
                }

                let sel = match state.active_multifile_files.get(&info_hash).cloned() {
                    Some(selection) if selection.file_idx == file_idx => {
                        if scheduled_generation == Some(selection.generation) {
                            state.active_multifile_files.remove(&info_hash);
                            tracing::info!(
                                info_hash = %info_hash,
                                file_idx,
                                generation = selection.generation,
                                reason = "delayed-cleanup",
                                "multifile_active_file_cleared"
                            );
                            None
                        } else {
                            tracing::debug!(
                                info_hash = %info_hash,
                                file_idx,
                                scheduled_generation,
                                active_generation = selection.generation,
                                "Skipping delayed cleanup because multi-file selection is newer"
                            );
                            return;
                        }
                    }
                    Some(selection) => Some(selection),
                    None => None,
                };

                if let Some((ref h, idx)) = state.active_file
                    && h == &info_hash
                    && idx == file_idx
                {
                    tracing::info!(
                        "Delayed cleanup: clearing active file for {} file_idx={}",
                        info_hash,
                        file_idx
                    );
                    state.active_file = sel
                        .as_ref()
                        .map(|selection| (info_hash.clone(), selection.file_idx));
                }

                sel
            };

            let engine = {
                let engines = engines.read().await;
                engines.get(&info_hash).cloned()
            };
            if let Some(engine) = engine {
                let reconciled = Self::reconcile_multifile_engine(
                    Arc::clone(&engine),
                    active_selection
                        .as_ref()
                        .map(|selection| selection.file_idx),
                    None,
                    active_selection
                        .as_ref()
                        .map(|selection| selection.generation)
                        .or(scheduled_generation)
                        .unwrap_or(0),
                    "delayed-cleanup",
                )
                .await;
                if reconciled {
                    tracing::info!(
                        info_hash = %info_hash,
                        file_idx,
                        active_file = active_selection.as_ref().map(|selection| selection.file_idx),
                        "Delayed cleanup: reconciled multi-file priorities"
                    );
                    return;
                }

                if let Err(e) = engine.handle.clear_file_streaming(file_idx).await {
                    tracing::warn!(
                        "Failed to clear file priorities for {} idx={}: {}",
                        info_hash,
                        file_idx,
                        e
                    );
                } else {
                    tracing::info!(
                        "Delayed cleanup: cleared file priorities for {} idx={}",
                        info_hash,
                        file_idx
                    );
                }
            }
        }))
    }

    /// Get a reference to the backend for direct access
    pub fn get_backend(&self) -> &Arc<B> {
        &self.backend
    }
}

#[cfg(feature = "libtorrent")]
impl BackendEngineFS<LibtorrentBackend> {
    pub async fn new(
        root_dir: std::path::PathBuf,
        config: crate::backend::BackendConfig,
    ) -> Result<Self> {
        Self::new_with_storage(root_dir, config, None).await
    }

    #[allow(
        clippy::unused_async,
        clippy::used_underscore_binding,
        clippy::unused_async_trait_impl
    )]
    pub async fn new_with_storage(
        root_dir: std::path::PathBuf,
        config: crate::backend::BackendConfig,
        tracker_storage: Option<Arc<dyn crate::trackers::TrackerStorage>>,
    ) -> Result<Self> {
        let download_dir = root_dir.join("libtorrent-downloads");
        let backend = LibtorrentBackend::new(download_dir.clone(), config)?;

        let efs = Self::new_with_backend_and_storage(
            backend,
            HashMap::new(),
            download_dir.clone(),
            download_dir,
            tracker_storage,
        );

        Ok(efs)
    }

    #[allow(clippy::unused_async, clippy::unused_async_trait_impl)]
    pub async fn new_disk_backed(
        root_dir: std::path::PathBuf,
        config: crate::backend::BackendConfig,
        tracker_storage: Option<Arc<dyn crate::trackers::TrackerStorage>>,
    ) -> Result<Self> {
        let download_dir = root_dir.join("torrent-cache");
        let backend = LibtorrentBackend::new_disk_backed(download_dir.clone(), config)?;

        Ok(Self::new_with_backend_and_storage(
            backend,
            HashMap::new(),
            download_dir.clone(),
            download_dir,
            tracker_storage,
        ))
    }

    /// Update session settings dynamically (called when user changes torrent profile)
    pub async fn update_speed_profile(&self, profile: &crate::backend::TorrentSpeedProfile) {
        self.backend
            .update_session_settings(profile, &crate::backend::TorrentPrivacyConfig::default())
            .await;
    }

    /// Update session settings dynamically (called when user changes torrent settings)
    pub async fn update_torrent_settings(
        &self,
        profile: &crate::backend::TorrentSpeedProfile,
        privacy: &crate::backend::TorrentPrivacyConfig,
    ) {
        self.backend.update_session_settings(profile, privacy).await;
    }

    pub fn set_seeding_enabled(&self, policy: impl Into<crate::backend::SeedingPolicy>) {
        let policy = policy.into();
        let enabled = policy.is_enabled();
        self.seeding_enabled.store(enabled, Ordering::Relaxed);
        self.backend.set_seeding_enabled(policy);
        tracing::info!(seeding_enabled = enabled, "Seeding policy updated");

        // When seeding is turned back on, resume torrents the seeding-disabled
        // policy had paused so they can seed again. Turning seeding off is
        // handled lazily by the periodic loop / schedule_torrent_pause.
        if enabled {
            let engines = Arc::clone(&self.engines);
            tokio::spawn(async move {
                let read = engines.read().await;
                for engine in read.values() {
                    if engine.handle.manages_playback_lifecycle() {
                        continue;
                    }
                    if engine.idle_paused.swap(false, Ordering::Relaxed)
                        && let Err(err) = engine.handle.resume_torrent().await
                    {
                        tracing::warn!(
                            info_hash = %engine.info_hash,
                            error = %err,
                            "Failed to resume torrent after re-enabling seeding"
                        );
                        engine.idle_paused.store(true, Ordering::Relaxed);
                    }
                }
            });
        }
    }

    pub fn seeding_enabled(&self) -> bool {
        self.seeding_enabled.load(Ordering::Relaxed)
    }

    /// Mark the torrent as active without pausing other active torrents.
    pub async fn focus_torrent(&self, target_info_hash: &str) {
        if self
            .get_engine(&target_info_hash.to_lowercase())
            .await
            .is_some_and(|engine| engine.handle.manages_playback_lifecycle())
        {
            return;
        }
        self.backend.set_streaming_mode(true).await;
        // focus_torrent resumes the torrent and reannounces to the swarm. Only
        // do that when the torrent still needs the swarm (not finished) or when
        // seeding is enabled. A finished torrent with seeding disabled is served
        // from disk and must stay paused so it is not re-seeded.
        let needs_swarm = match self.get_engine(&target_info_hash.to_lowercase()).await {
            Some(engine) => !engine.handle.is_finished().await,
            None => false,
        };
        if needs_swarm || self.seeding_enabled.load(Ordering::Relaxed) {
            self.backend.focus_torrent(target_info_hash).await;
        }
    }

    /// Resume all paused torrents (called when streaming ends)
    /// Also disables streaming mode (restores normal upload)
    pub async fn resume_all_torrents(&self) {
        self.backend.resume_all_torrents().await;
    }

    /// Pause all torrents (called when no active streams remain)
    pub async fn pause_all_torrents(&self) {
        self.backend.pause_all_torrents().await;
    }

    /// Enable or disable streaming mode (limits uploads during streaming)
    #[allow(clippy::fn_params_excessive_bools)]
    pub async fn set_streaming_mode(&self, enabled: bool) {
        self.backend.set_streaming_mode(enabled).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        BackendFileInfo, EngineStats, FileStreamTrait, Growler, PeerSearch, PieceReadiness,
        StatsFile, StatsOptions, SwarmCap, TorrentFilePriorityPlan,
    };
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const TEST_HASH: &str = "0123456789abcdef0123456789abcdef01234567";

    #[derive(Default)]
    struct FakeCounters {
        keep_file_downloading: AtomicUsize,
        clear_file_streaming: AtomicUsize,
        resume_torrent: AtomicUsize,
        pause_torrent: AtomicUsize,
        reconcile_file_priorities: AtomicUsize,
        last_active_file: Mutex<Option<usize>>,
        last_generation: AtomicU64,
    }

    #[derive(Clone)]
    struct FakeHandle {
        info_hash: String,
        counters: Arc<FakeCounters>,
        file_count: usize,
    }

    struct FakeBackend {
        handle: FakeHandle,
    }

    #[async_trait::async_trait]
    impl TorrentBackend for FakeBackend {
        type Handle = FakeHandle;

        async fn add_torrent(
            &self,
            _source: TorrentSource,
            _trackers: Vec<String>,
        ) -> Result<Self::Handle> {
            Ok(self.handle.clone())
        }

        async fn get_torrent(&self, info_hash: &str) -> Option<Self::Handle> {
            (info_hash == self.handle.info_hash).then(|| self.handle.clone())
        }

        async fn remove_torrent(&self, _info_hash: &str) -> Result<()> {
            Ok(())
        }

        async fn list_torrents(&self) -> Vec<String> {
            vec![self.handle.info_hash.clone()]
        }

        async fn memory_diagnostics(&self) -> BackendMemoryDiagnostics {
            BackendMemoryDiagnostics::default()
        }
    }

    #[async_trait::async_trait]
    impl TorrentHandle for FakeHandle {
        fn info_hash(&self) -> String {
            self.info_hash.clone()
        }

        fn name(&self) -> Option<String> {
            Some("fake".to_string())
        }

        async fn stats(&self) -> EngineStats {
            let files = (0..self.file_count)
                .map(|idx| StatsFile {
                    name: format!("video-{idx}.mkv"),
                    path: format!("video-{idx}.mkv"),
                    length: 100,
                    offset: idx as u64 * 100,
                    downloaded: 50,
                    progress: 0.5,
                })
                .collect();
            EngineStats {
                name: "fake".to_string(),
                info_hash: self.info_hash.clone(),
                files,
                sources: vec![],
                opts: StatsOptions {
                    connections: None,
                    dht: true,
                    growler: Growler::default(),
                    handshake_timeout: None,
                    path: String::new(),
                    peer_search: PeerSearch::default(),
                    swarm_cap: SwarmCap::default(),
                    timeout: None,
                    tracker: true,
                    r#virtual: false,
                },
                download_speed: 0.0,
                upload_speed: 0.0,
                downloaded: 50,
                uploaded: 0,
                unchoked: 0,
                peers: 0,
                queued: 0,
                unique: 0,
                connection_tries: 0,
                peer_search_running: false,
                stream_len: 100,
                stream_name: "video.mkv".to_string(),
                stream_progress: 0.5,
                swarm_connections: 0,
                swarm_paused: false,
                swarm_size: 0,
                is_finished: false,
                has_metadata: self.file_count > 0,
            }
        }

        async fn add_trackers(&self, _trackers: Vec<String>) -> Result<()> {
            Ok(())
        }

        async fn resume_torrent(&self) -> Result<()> {
            self.counters.resume_torrent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn pause_torrent(&self) -> Result<()> {
            self.counters.pause_torrent.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn keep_file_downloading(&self, _file_idx: usize) -> Result<()> {
            self.counters
                .keep_file_downloading
                .fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn reconcile_file_priorities(&self, plan: TorrentFilePriorityPlan) -> Result<()> {
            self.counters
                .reconcile_file_priorities
                .fetch_add(1, Ordering::SeqCst);
            *self.counters.last_active_file.lock().unwrap() = plan.active_file;
            self.counters
                .last_generation
                .store(plan.generation, Ordering::SeqCst);
            Ok(())
        }

        async fn get_file_reader(
            &self,
            _file_idx: usize,
            _start_offset: u64,
            _priority: u8,
            _bitrate: Option<u64>,
            _intent: crate::backend::priorities::PlaybackIntent,
        ) -> Result<Box<dyn FileStreamTrait>> {
            anyhow::bail!("not implemented")
        }

        async fn get_files(&self) -> Vec<BackendFileInfo> {
            (0..self.file_count)
                .map(|idx| BackendFileInfo {
                    name: format!("video-{idx}.mkv"),
                    length: 100,
                })
                .collect()
        }

        async fn get_file_path(&self, _file_idx: usize) -> Option<String> {
            None
        }

        async fn prepare_file_for_streaming(&self, _file_idx: usize) -> Result<()> {
            Ok(())
        }

        async fn clear_file_streaming(&self, _file_idx: usize) -> Result<()> {
            self.counters
                .clear_file_streaming
                .fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn wait_for_piece_ready(
            &self,
            _file_idx: usize,
            _offset: u64,
            _timeout: Duration,
            _intent: crate::backend::priorities::PlaybackIntent,
        ) -> Result<PieceReadiness> {
            Ok(PieceReadiness {
                ready: true,
                piece: 0,
                ready_pieces: 1,
                target_pieces: 1,
                elapsed_ms: 0,
                peers: 0,
                download_rate: 0,
                reason: "fake".to_string(),
            })
        }
    }

    fn test_enginefs() -> (BackendEngineFS<FakeBackend>, Arc<FakeCounters>) {
        test_enginefs_with_file_count(1)
    }

    fn test_enginefs_with_file_count(
        file_count: usize,
    ) -> (BackendEngineFS<FakeBackend>, Arc<FakeCounters>) {
        let counters = Arc::new(FakeCounters::default());
        let handle = FakeHandle {
            info_hash: TEST_HASH.to_string(),
            counters: counters.clone(),
            file_count,
        };
        let mut restored = HashMap::new();
        restored.insert(TEST_HASH.to_string(), handle.clone());
        let root = std::env::temp_dir().join("enginefs-hls-lease-tests");
        let enginefs = BackendEngineFS::new_with_backend(
            FakeBackend { handle },
            restored,
            root.join("cache"),
            root.join("downloads"),
        );
        (enginefs, counters)
    }

    #[tokio::test]
    async fn refresh_existing_hls_playback_does_not_create_lease() {
        let (enginefs, counters) = test_enginefs();

        let refreshed = enginefs
            .refresh_existing_hls_playback(TEST_HASH, 0, "stats-json")
            .await;

        assert!(!refreshed);
        assert_eq!(counters.keep_file_downloading.load(Ordering::SeqCst), 0);
        assert!(
            enginefs
                .stream_activity_snapshot()
                .await
                .active_playback_leases
                .is_empty()
        );
    }

    #[tokio::test]
    async fn refresh_hls_playback_creates_lease_and_keeps_file_wanted() {
        let (enginefs, counters) = test_enginefs();

        enginefs.refresh_hls_playback(TEST_HASH, 0, "test").await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert_eq!(snapshot.active_playback_leases.len(), 1);
        assert_eq!(counters.keep_file_downloading.load(Ordering::SeqCst), 1);
        assert_eq!(counters.resume_torrent.load(Ordering::SeqCst), 1);
        assert_eq!(snapshot.active_file.map(|active| active.file_idx), Some(0));
    }

    #[tokio::test]
    async fn multi_file_selects_only_requested_file() {
        let (enginefs, counters) = test_enginefs_with_file_count(3);

        enginefs.on_stream_start(TEST_HASH, 1).await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert_eq!(snapshot.active_multifile_selections.len(), 1);
        assert_eq!(snapshot.active_multifile_selections[0].file_idx, 1);
        assert_eq!(*counters.last_active_file.lock().unwrap(), Some(1));
        assert_eq!(counters.keep_file_downloading.load(Ordering::SeqCst), 0);
        assert_eq!(counters.reconcile_file_priorities.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn multi_file_latest_request_wins() {
        let (enginefs, counters) = test_enginefs_with_file_count(3);

        enginefs.on_stream_start(TEST_HASH, 1).await;
        enginefs.on_stream_start(TEST_HASH, 2).await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert_eq!(snapshot.active_multifile_selections.len(), 1);
        assert_eq!(snapshot.active_multifile_selections[0].file_idx, 2);
        assert_eq!(*counters.last_active_file.lock().unwrap(), Some(2));
        assert!(
            snapshot
                .active_file_streams
                .iter()
                .all(|stream| stream.file_idx == 2)
        );
        assert_eq!(counters.reconcile_file_priorities.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn hls_lease_switch_removes_sibling_lease() {
        let (enginefs, _counters) = test_enginefs_with_file_count(3);

        enginefs.refresh_hls_playback(TEST_HASH, 1, "test").await;
        enginefs.refresh_hls_playback(TEST_HASH, 2, "test").await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert_eq!(snapshot.active_playback_leases.len(), 1);
        assert_eq!(snapshot.active_playback_leases[0].file_idx, 2);
        assert_eq!(snapshot.active_multifile_selections.len(), 1);
        assert_eq!(snapshot.active_multifile_selections[0].file_idx, 2);
    }

    #[tokio::test]
    async fn stats_cannot_switch_active_multifile_file() {
        let (enginefs, _counters) = test_enginefs_with_file_count(3);

        enginefs.refresh_hls_playback(TEST_HASH, 1, "test").await;
        let refreshed = enginefs
            .refresh_existing_hls_playback(TEST_HASH, 2, "stats-json")
            .await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert!(!refreshed);
        assert_eq!(snapshot.active_multifile_selections.len(), 1);
        assert_eq!(snapshot.active_multifile_selections[0].file_idx, 1);
        assert_eq!(snapshot.active_playback_leases.len(), 1);
        assert_eq!(snapshot.active_playback_leases[0].file_idx, 1);
    }

    #[tokio::test]
    async fn stats_refreshes_current_multifile_file() {
        let (enginefs, _counters) = test_enginefs_with_file_count(3);

        enginefs.refresh_hls_playback(TEST_HASH, 1, "test").await;
        let refreshed = enginefs
            .refresh_existing_hls_playback(TEST_HASH, 1, "stats-json")
            .await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert!(refreshed);
        assert_eq!(snapshot.active_multifile_selections.len(), 1);
        assert_eq!(snapshot.active_multifile_selections[0].file_idx, 1);
        assert_eq!(snapshot.active_playback_leases.len(), 1);
        assert_eq!(snapshot.active_playback_leases[0].file_idx, 1);
    }

    #[tokio::test]
    async fn old_cleanup_cannot_clear_newer_multifile_active_file() {
        let (enginefs, counters) = test_enginefs_with_file_count(3);

        enginefs.refresh_hls_playback(TEST_HASH, 1, "test").await;
        enginefs
            .schedule_file_cleanup(TEST_HASH.to_string(), 1)
            .await;
        enginefs.refresh_hls_playback(TEST_HASH, 2, "test").await;
        tokio::time::sleep(Duration::from_secs(6)).await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert_eq!(snapshot.active_multifile_selections.len(), 1);
        assert_eq!(snapshot.active_multifile_selections[0].file_idx, 2);
        assert_eq!(*counters.last_active_file.lock().unwrap(), Some(2));
        assert_eq!(counters.clear_file_streaming.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn paused_torrent_resumes_for_requested_multifile_file() {
        let (enginefs, counters) = test_enginefs_with_file_count(3);
        {
            let engine = enginefs.get_engine(TEST_HASH).await.unwrap();
            engine.idle_paused.store(true, Ordering::Relaxed);
        }

        enginefs.on_stream_start(TEST_HASH, 2).await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert_eq!(snapshot.active_multifile_selections.len(), 1);
        assert_eq!(snapshot.active_multifile_selections[0].file_idx, 2);
        assert_eq!(*counters.last_active_file.lock().unwrap(), Some(2));
        assert!(
            counters.resume_torrent.load(Ordering::SeqCst) > 0,
            "request should resume an idle-paused torrent"
        );
    }

    #[tokio::test]
    async fn idle_pause_skips_active_multifile_selection() {
        let (enginefs, counters) = test_enginefs_with_file_count(3);
        enginefs.seeding_enabled.store(false, Ordering::Relaxed);

        enginefs.on_stream_start(TEST_HASH, 1).await;
        {
            let mut state = enginefs.stream_state.write().await;
            state.active_streams.clear();
            state.active_file_streams.clear();
        }
        enginefs
            .schedule_torrent_pause_after(TEST_HASH.to_string(), Duration::from_millis(10))
            .await
            .unwrap();

        assert_eq!(counters.pause_torrent.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn idle_pause_runs_when_no_activity_remains() {
        let (enginefs, counters) = test_enginefs_with_file_count(3);
        enginefs.seeding_enabled.store(false, Ordering::Relaxed);

        enginefs
            .schedule_torrent_pause_after(TEST_HASH.to_string(), Duration::from_millis(10))
            .await
            .unwrap();

        assert_eq!(counters.pause_torrent.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn idle_pause_skips_torrent_awaiting_metadata() {
        let (enginefs, counters) = test_enginefs_with_file_count(0);
        enginefs.seeding_enabled.store(false, Ordering::Relaxed);

        enginefs
            .schedule_torrent_pause_after(TEST_HASH.to_string(), Duration::from_millis(10))
            .await
            .unwrap();

        assert_eq!(counters.pause_torrent.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn single_file_bypasses_multifile_selector() {
        let (enginefs, counters) = test_enginefs();

        enginefs.on_stream_start(TEST_HASH, 0).await;

        let snapshot = enginefs.stream_activity_snapshot().await;
        assert!(snapshot.active_multifile_selections.is_empty());
        assert_eq!(counters.reconcile_file_priorities.load(Ordering::SeqCst), 0);
        assert_eq!(counters.keep_file_downloading.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn active_hls_lease_prevents_delayed_cleanup() {
        let (enginefs, counters) = test_enginefs();

        enginefs.refresh_hls_playback(TEST_HASH, 0, "test").await;
        let cleanup = enginefs
            .schedule_file_cleanup_after(TEST_HASH.to_string(), 0, Duration::from_millis(10))
            .await
            .expect("cleanup task");
        cleanup.await.expect("cleanup task completed");

        assert_eq!(counters.clear_file_streaming.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn expired_hls_lease_allows_delayed_cleanup() {
        let (enginefs, counters) = test_enginefs();

        enginefs.refresh_hls_playback(TEST_HASH, 0, "test").await;
        {
            let mut state = enginefs.stream_state.write().await;
            state
                .active_playback_leases
                .get_mut(&(TEST_HASH.to_string(), 0))
                .unwrap()
                .expires_at_secs = now_secs();
        }
        let cleanup = enginefs
            .schedule_file_cleanup_after(TEST_HASH.to_string(), 0, Duration::from_millis(10))
            .await
            .expect("cleanup task");
        cleanup.await.expect("cleanup task completed");

        assert_eq!(counters.clear_file_streaming.load(Ordering::SeqCst), 1);
    }
}
