use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::analysis::{analysis_version, analyze_file, apply_analysis, ANALYSIS_VERSION};
use crate::metadata::{has_audio_extension, probe_metadata};
use crate::model::{canonicalish_string, fingerprint, Library, Track};
use crate::paths::{lexical_absolute, normalize_folder, path_is_within, AppPaths};
use crate::similarity::build_similarity_graph;
use crate::store::{log_scan_summary, save_library, Tracker};
use crate::watcher::{collect_snapshot, diff, library_baseline, FileSignature};

#[derive(Clone, Debug)]
pub enum WorkerCommand {
    Scan {
        roots: Vec<String>,
        prune: bool,
        force_analysis: bool,
    },
    SetWatch {
        enabled: bool,
        roots: Vec<String>,
    },
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct ProgressInfo {
    pub stage: String,
    pub processed: usize,
    pub total: usize,
    pub message: String,
    pub current: String,
    pub current_name: String,
    pub elapsed: f64,
    pub failed: usize,
    pub edges: usize,
    pub percent: f64,
}

#[derive(Clone, Debug)]
pub struct ScanOutcome {
    pub summary: String,
    pub removed_ids: Vec<String>,
    pub tracks: usize,
    pub edges: usize,
    pub loaded: usize,
    pub updated: usize,
    pub analyzed: usize,
    pub failed: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug)]
pub enum WorkerMessage {
    Started { auto: bool, message: String },
    WatchTriggered(String),
    Progress(ProgressInfo),
    Finished(ScanOutcome),
    Error(String),
}

pub struct WorkerHandle {
    tx: Sender<WorkerCommand>,
    rx: Receiver<WorkerMessage>,
    cancel: Arc<AtomicBool>,
    done_rx: Receiver<()>,
    join: Option<JoinHandle<()>>,
}

impl WorkerHandle {
    pub fn spawn(paths: AppPaths, library: Arc<RwLock<Library>>) -> Self {
        let (tx, command_rx) = channel();
        let (message_tx, rx) = channel();
        let (done_tx, done_rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = Arc::clone(&cancel);
        let join = thread::Builder::new()
            .name("gbonds-library".to_string())
            .spawn(move || {
                worker_loop(
                    paths,
                    library,
                    command_rx,
                    message_tx,
                    cancel_thread,
                    done_tx,
                );
            })
            .ok();
        Self {
            tx,
            rx,
            cancel,
            done_rx,
            join,
        }
    }

    pub fn send(&self, command: WorkerCommand) {
        let _ = self.tx.send(command);
    }

    pub fn drain(&self) -> Vec<WorkerMessage> {
        self.rx.try_iter().collect()
    }

    pub fn shutdown(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.tx.send(WorkerCommand::Shutdown);
        let finished = self
            .done_rx
            .recv_timeout(Duration::from_millis(1600))
            .is_ok();
        if finished {
            if let Some(join) = self.join.take() {
                let _ = join.join();
            }
        } else {
            // A single ffmpeg/analysis pass can outlive the UI. Leave the
            // thread detached instead of blocking the quit path; the process
            // exits right after this and all file writes are atomic.
            self.join.take();
        }
    }
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        let _ = self.tx.send(WorkerCommand::Shutdown);
    }
}

fn watch_interval() -> Duration {
    let raw = std::env::var("GBONDS_WATCH_INTERVAL")
        .or_else(|_| std::env::var("TUI_MUSIC_WATCH_INTERVAL"))
        .unwrap_or_else(|_| "5".to_string());
    let seconds = raw.parse::<f64>().unwrap_or(5.0).max(1.0);
    Duration::from_secs_f64(seconds)
}

fn worker_loop(
    paths: AppPaths,
    library: Arc<RwLock<Library>>,
    command_rx: Receiver<WorkerCommand>,
    message_tx: Sender<WorkerMessage>,
    cancel: Arc<AtomicBool>,
    done_tx: Sender<()>,
) {
    let mut watch_enabled = false;
    let mut watch_roots: Vec<String> = Vec::new();
    let mut baseline: Option<HashMap<String, FileSignature>> = None;
    let interval = watch_interval();
    let mut last_watch = Instant::now();

    loop {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        match command_rx.recv_timeout(Duration::from_millis(400)) {
            Ok(WorkerCommand::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break;
            }
            Ok(WorkerCommand::SetWatch { enabled, roots }) => {
                watch_enabled = enabled;
                watch_roots = roots
                    .into_iter()
                    .map(|root| normalize_folder(root))
                    .collect();
                baseline = None;
                last_watch = Instant::now();
            }
            Ok(WorkerCommand::Scan {
                roots,
                prune,
                force_analysis,
            }) => {
                if roots.is_empty() {
                    let _ = message_tx.send(WorkerMessage::Error(
                        "No music folders configured. Press o to add one.".to_string(),
                    ));
                } else {
                    perform_scan(
                        &paths,
                        &library,
                        &message_tx,
                        &cancel,
                        roots,
                        prune,
                        force_analysis,
                        false,
                    );
                }
                if watch_enabled && !watch_roots.is_empty() {
                    baseline = Some(collect_snapshot(&watch_roots).files);
                    last_watch = Instant::now();
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }

        if cancel.load(Ordering::SeqCst) {
            break;
        }
        if watch_enabled && !watch_roots.is_empty() && last_watch.elapsed() >= interval {
            last_watch = Instant::now();
            let snapshot = collect_snapshot(&watch_roots);
            let previous = match baseline.take() {
                Some(baseline) => baseline,
                None => {
                    let expected = {
                        let library = match library.read() {
                            Ok(library) => library,
                            Err(_) => {
                                let _ = message_tx.send(WorkerMessage::Error(
                                    "Library lock is poisoned.".to_string(),
                                ));
                                break;
                            }
                        };
                        let tracks = library.tracks.iter().filter_map(|track| {
                            let path = track.filepath.as_ref()?;
                            Some((canonicalish_string(path), track.signature()))
                        });
                        library_baseline(tracks, &snapshot.roots)
                    };
                    expected
                }
            };

            let change = diff(
                &previous,
                &snapshot.files,
                &snapshot.roots,
                &snapshot.root_errors,
            );

            let protected_roots: HashSet<String> = snapshot
                .root_errors
                .iter()
                .cloned()
                .chain(
                    snapshot
                        .roots
                        .iter()
                        .filter(|(_, exists)| !**exists)
                        .map(|(root, _)| root.clone()),
                )
                .collect();

            let mut next_baseline = snapshot.files.clone();
            if !protected_roots.is_empty() {
                for (path, signature) in previous {
                    if next_baseline.contains_key(&path) {
                        continue;
                    }
                    if protected_roots
                        .iter()
                        .any(|root| path_is_within(&path, root))
                    {
                        next_baseline.insert(path, signature);
                    }
                }
            }
            baseline = Some(next_baseline);

            if change.has_changes() {
                let message = format!(
                    "Folder changes: {} new, {} changed, {} removed",
                    change.added.len(),
                    change.modified.len(),
                    change.removed.len()
                );
                let _ = message_tx.send(WorkerMessage::WatchTriggered(message));
                perform_scan(
                    &paths,
                    &library,
                    &message_tx,
                    &cancel,
                    watch_roots.clone(),
                    true,
                    false,
                    true,
                );
                baseline = Some(collect_snapshot(&watch_roots).files);
                last_watch = Instant::now();
            }
        }
    }
    let _ = done_tx.send(());
}

fn send_progress(
    tx: &Sender<WorkerMessage>,
    stage: &str,
    processed: usize,
    total: usize,
    message: &str,
    current: &str,
    current_name: &str,
    elapsed: f64,
    failed: usize,
    edges: usize,
    percent: f64,
) {
    let _ = tx.send(WorkerMessage::Progress(ProgressInfo {
        stage: stage.to_string(),
        processed,
        total,
        message: message.to_string(),
        current: current.to_string(),
        current_name: current_name.to_string(),
        elapsed,
        failed,
        edges,
        percent,
    }));
}

fn save_snapshot(library: &Arc<RwLock<Library>>, path: &Path) -> Result<usize, String> {
    let library = library
        .read()
        .map_err(|_| "Library lock is poisoned.".to_string())?;
    let report = save_library(&library, path).map_err(|error| error.to_string())?;
    Ok(report.edges)
}

/// Scans the given roots, imports/updates tracks, analyzes missing audio and
/// rebuilds the similarity graph. UI reads stay available because the write
/// lock is only held for short metadata updates, never for ffmpeg work.
fn perform_scan(
    paths: &AppPaths,
    library: &Arc<RwLock<Library>>,
    message_tx: &Sender<WorkerMessage>,
    cancel: &AtomicBool,
    roots: Vec<String>,
    prune: bool,
    force_analysis: bool,
    auto: bool,
) {
    let started = Instant::now();
    let roots: Vec<String> = roots
        .into_iter()
        .map(|root| normalize_folder(root))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    let _ = message_tx.send(WorkerMessage::Started {
        auto,
        message: if auto {
            "Applying folder changes".to_string()
        } else {
            "Scanning music folders".to_string()
        },
    });

    let mut tracker = Tracker::start(
        paths.analysis_state.clone(),
        "scan",
        0,
        "searching for audio files",
    );
    let mut lines: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut loaded = 0usize;
    let mut updated = 0usize;
    let mut skipped = 0usize;
    let mut analyzed = 0usize;
    let mut failed = 0usize;
    let mut removed_ids: Vec<String> = Vec::new();
    let mut edges = 0usize;
    let mut average_weight = 0.0_f64;
    let mut save_error: Option<String> = None;

    tracker.update(
        Some("scan"),
        Some(0),
        Some(0),
        None,
        None,
        Some("searching for audio files"),
        true,
    );
    send_progress(
        message_tx,
        "scan",
        0,
        0,
        "searching for audio files",
        "",
        "",
        0.0,
        0,
        0,
        0.0,
    );

    let snapshot = collect_snapshot(&roots);
    let current_signatures = snapshot.files.clone();
    let current_paths: HashSet<String> = current_signatures.keys().cloned().collect();
    let existing_roots: HashSet<String> = snapshot
        .roots
        .iter()
        .filter(|(_, exists)| **exists)
        .map(|(root, _)| root.clone())
        .collect();
    let mut files: Vec<String> = current_paths.iter().cloned().collect();
    files.sort();

    for (root, exists) in &snapshot.roots {
        if !exists {
            lines.push(format!("Folder not found: {root}"));
        }
    }
    if !snapshot.errors.is_empty() {
        errors.extend(snapshot.errors.iter().take(5).cloned());
    }

    // Remove entries whose files disappeared. A temporarily unavailable root
    // is protected: an unplugged disk must not empty the library.
    if prune && !existing_roots.is_empty() {
        if let Ok(mut library) = library.write() {
            let candidates: Vec<String> = library
                .tracks
                .iter()
                .filter(|track| {
                    let Some(path) = track.filepath.as_ref() else {
                        return false;
                    };
                    if !existing_roots.iter().any(|root| path_is_within(path, root)) {
                        return false;
                    }
                    !current_paths.contains(&canonicalish_string(path))
                })
                .map(|track| track.id.clone())
                .collect();
            for id in candidates {
                if library.remove_track(&id).is_some() {
                    removed_ids.push(id);
                }
            }
            if !removed_ids.is_empty() {
                library.bump();
            }
        }
    }

    // Fill missing file signatures for caches written by older versions.
    if let Ok(mut library) = library.write() {
        for track in &mut library.tracks {
            let Some(path) = track.filepath.as_ref() else {
                continue;
            };
            let path = canonicalish_string(path);
            if !existing_roots
                .iter()
                .any(|root| path_is_within(&path, root))
            {
                continue;
            }
            if track.file_mtime_ns.is_none() || track.file_size.is_none() {
                if let Some((mtime, size)) = current_signatures.get(&path) {
                    track.file_mtime_ns = Some(*mtime);
                    track.file_size = Some(*size);
                }
            }
        }
        library.bump();
    }

    let (mut path_to_id, mut fingerprints, mut next_id) = {
        let library = match library.read() {
            Ok(library) => library,
            Err(_) => {
                let _ = message_tx.send(WorkerMessage::Error(
                    "Library lock is poisoned.".to_string(),
                ));
                return;
            }
        };
        let mut path_to_id = HashMap::new();
        for track in &library.tracks {
            if let Some(path) = track.filepath.as_ref() {
                path_to_id.insert(canonicalish_string(path), track.id.clone());
            }
        }
        let fingerprints = library.existing_fingerprints();
        let max_id = library.max_numeric_id();
        (path_to_id, fingerprints, max_id + 1)
    };

    let total_files = files.len();
    tracker.update(
        Some("scan"),
        Some(0),
        Some(total_files),
        None,
        None,
        Some("reading tags"),
        true,
    );

    for (index, path) in files.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let file_path = Path::new(path);
        let known_id = path_to_id.get(path).cloned();
        let signature = current_signatures.get(path).copied();

        tracker.update(
            Some("scan"),
            Some(index),
            Some(total_files),
            Some(path),
            None,
            Some("reading tags"),
            false,
        );
        if index % 3 == 0 || index + 1 == total_files {
            let percent = if total_files > 0 {
                index as f64 * 100.0 / total_files as f64
            } else {
                0.0
            };
            send_progress(
                message_tx,
                "scan",
                index,
                total_files,
                "reading tags",
                path,
                Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default()
                    .as_str(),
                started.elapsed().as_secs_f64(),
                failed,
                0,
                percent,
            );
        }

        if let Some(id) = known_id {
            let current_signature = {
                let library = match library.read() {
                    Ok(library) => library,
                    Err(_) => break,
                };
                library.find(&id).and_then(Track::signature)
            };
            match (current_signature, signature) {
                (None, Some(signature)) => {
                    if let Ok(mut library) = library.write() {
                        if let Some(track) = library.find_mut(&id) {
                            track.file_mtime_ns = Some(signature.0);
                            track.file_size = Some(signature.1);
                        }
                    }
                }
                (Some(previous), Some(signature)) if previous != signature => {
                    match probe_metadata(file_path) {
                        Ok(meta) => {
                            if let Ok(mut library) = library.write() {
                                if let Some(track) = library.find_mut(&id) {
                                    track.title = meta.title;
                                    track.artist = meta.artist;
                                    track.genre = meta.genre.clone();
                                    track.metadata_genre = meta.genre;
                                    track.album = Some(meta.album);
                                    track.year = meta.year;
                                    track.track_number = meta.track_number;
                                    track.duration = meta.duration;
                                    track.file_mtime_ns = Some(signature.0);
                                    track.file_size = Some(signature.1);
                                    track.analysis = None;
                                    library.bump();
                                    updated += 1;
                                }
                            }
                        }
                        Err(error) => errors.push(error),
                    }
                }
                _ => {}
            }
            continue;
        }

        let meta = match probe_metadata(file_path) {
            Ok(meta) => meta,
            Err(error) => {
                if has_audio_extension(file_path) {
                    errors.push(error);
                } else {
                    skipped += 1;
                }
                continue;
            }
        };
        let candidate = Track {
            artist: meta.artist.clone(),
            title: meta.title.clone(),
            album: Some(meta.album.clone()),
            duration: meta.duration,
            ..Track::default()
        };
        let candidate_fingerprint = fingerprint(&candidate);
        if candidate.album.as_deref() != Some("Unknown")
            && fingerprints.contains(&candidate_fingerprint)
        {
            skipped += 1;
            continue;
        }

        let id = {
            let library = match library.read() {
                Ok(library) => library,
                Err(_) => break,
            };
            let candidate = library.next_free_id(next_id);
            candidate.to_string()
        };
        next_id = next_id.max(id.parse::<i64>().unwrap_or(next_id)) + 1;

        let absolute_path = lexical_absolute(file_path).to_string_lossy().into_owned();
        let track = Track {
            id: id.clone(),
            title: meta.title,
            artist: meta.artist,
            genre: meta.genre.clone(),
            metadata_genre: meta.genre,
            duration: meta.duration,
            rating: 5,
            filepath: Some(absolute_path),
            album: Some(meta.album),
            year: meta.year,
            track_number: meta.track_number,
            file_mtime_ns: signature.map(|value| value.0),
            file_size: signature.map(|value| value.1),
            analysis: None,
            ..Track::default()
        };
        let mut added = false;
        if let Ok(mut library) = library.write() {
            if library.add_track(track) {
                added = true;
                library.bump();
            }
        }
        if added {
            path_to_id.insert(path.clone(), id);
            fingerprints.insert(candidate_fingerprint);
            loaded += 1;
        } else {
            skipped += 1;
        }
    }

    if let Ok(mut library) = library.write() {
        library.bump();
    }

    // Analysis pass.
    let targets: Vec<(String, String)> = {
        let library = match library.read() {
            Ok(library) => library,
            Err(_) => {
                let _ = message_tx.send(WorkerMessage::Error(
                    "Library lock is poisoned.".to_string(),
                ));
                return;
            }
        };
        library
            .tracks
            .iter()
            .filter(|track| track.filepath.is_some())
            .filter(|track| {
                force_analysis
                    || track.analysis.is_none()
                    || track
                        .analysis
                        .as_ref()
                        .map(|analysis| analysis_version(analysis) < ANALYSIS_VERSION)
                        .unwrap_or(true)
            })
            .filter(|track| {
                track
                    .filepath
                    .as_deref()
                    .map(|path| Path::new(path).is_file())
                    .unwrap_or(false)
            })
            .map(|track| (track.id.clone(), track.filepath.clone().unwrap_or_default()))
            .collect()
    };
    let total_analysis = targets.len();
    tracker.update(
        Some("analyze"),
        Some(0),
        Some(total_analysis),
        None,
        None,
        Some("analyzing audio"),
        true,
    );
    tracker.set_extra("force", json!(force_analysis));
    send_progress(
        message_tx,
        "analyze",
        0,
        total_analysis,
        "analyzing audio",
        "",
        "",
        started.elapsed().as_secs_f64(),
        0,
        0,
        0.0,
    );

    for (index, (id, path)) in targets.iter().enumerate() {
        if cancel.load(Ordering::SeqCst) {
            break;
        }
        let (display, name) = {
            let library = match library.read() {
                Ok(library) => library,
                Err(_) => break,
            };
            match library.find(id) {
                Some(track) => (track.title_artist(), track.title.clone()),
                None => (id.clone(), String::new()),
            }
        };
        tracker.update(
            Some("analyze"),
            Some(index),
            Some(total_analysis),
            Some(path),
            Some(&display),
            Some("analyzing audio"),
            false,
        );
        tracker.set_extra("failed", json!(failed));
        send_progress(
            message_tx,
            "analyze",
            index,
            total_analysis,
            "analyzing audio",
            path,
            &name,
            started.elapsed().as_secs_f64(),
            failed,
            0,
            if total_analysis > 0 {
                index as f64 * 100.0 / total_analysis as f64
            } else {
                0.0
            },
        );

        match analyze_file(Path::new(path)) {
            Ok(analysis) => {
                if let Ok(mut library) = library.write() {
                    if let Some(track) = library.find_mut(id) {
                        apply_analysis(track, analysis);
                        library.bump();
                        analyzed += 1;
                    }
                }
            }
            Err(error) => {
                failed += 1;
                errors.push(format!("{display}: {error}"));
            }
        }

        if (index + 1) % 20 == 0 {
            let _ = save_snapshot(library, &paths.library);
        }
    }
    tracker.update(
        Some("analyze"),
        Some(total_analysis),
        Some(total_analysis),
        None,
        None,
        Some("audio analysis complete"),
        true,
    );
    tracker.set_extra("analyzed", json!(analyzed));
    tracker.set_extra("failed", json!(failed));

    // Graph rebuild.
    if !cancel.load(Ordering::SeqCst) {
        tracker.update(
            Some("graph"),
            Some(0),
            Some(1),
            None,
            None,
            Some("building similarity graph"),
            true,
        );
        send_progress(
            message_tx,
            "graph",
            0,
            1,
            "building similarity graph",
            "",
            "",
            started.elapsed().as_secs_f64(),
            failed,
            edges,
            0.0,
        );
        let (new_edges, stats) = {
            let library = match library.read() {
                Ok(library) => library,
                Err(_) => {
                    let _ = message_tx.send(WorkerMessage::Error(
                        "Library lock is poisoned.".to_string(),
                    ));
                    return;
                }
            };
            build_similarity_graph(&library, 6, 0.5)
        };
        edges = stats.edges;
        average_weight = stats.average_weight;
        if let Ok(mut library) = library.write() {
            let nodes = library
                .tracks
                .iter()
                .map(|track| track.id.clone())
                .collect::<Vec<_>>();
            library.graph.replace(nodes, new_edges);
            library.bump();
        }
        tracker.set_extra("tracks", json!(stats.tracks));
        tracker.set_extra("edges", json!(stats.edges));
        send_progress(
            message_tx,
            "graph",
            1,
            1,
            "similarity graph ready",
            "",
            "",
            started.elapsed().as_secs_f64(),
            failed,
            edges,
            100.0,
        );
    }

    let track_count = library
        .read()
        .map(|library| library.tracks.len())
        .unwrap_or(0);

    // Save.
    if !cancel.load(Ordering::SeqCst) {
        tracker.update(
            Some("save"),
            Some(0),
            Some(1),
            None,
            None,
            Some("saving library"),
            true,
        );
        if let Err(error) = save_snapshot(library, &paths.library) {
            save_error = Some(error.clone());
            errors.push(format!("Cannot save library: {error}"));
        }
        tracker.update(
            Some("save"),
            Some(1),
            Some(1),
            None,
            None,
            Some("library saved"),
            true,
        );
    }

    lines.insert(0, format!("Loaded tracks: {loaded}"));
    if updated > 0 {
        lines.push(format!("Updated tracks: {updated}"));
    }
    if skipped > 0 {
        lines.push(format!("Skipped (duplicates/non-audio): {skipped}"));
    }
    if analyzed > 0 {
        lines.push(format!("Analyzed: {analyzed}"));
    }
    if failed > 0 {
        lines.push(format!("Analysis failures: {failed}"));
    }
    if !removed_ids.is_empty() {
        lines.push(format!("Removed missing files: {}", removed_ids.len()));
    }
    lines.push(format!("Graph edges: {edges}"));
    if edges > 0 {
        lines.push(format!("Average weight: {average_weight:.3}"));
    }
    if !errors.is_empty() {
        lines.push(format!(
            "Errors: {}",
            errors
                .iter()
                .take(3)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let elapsed = started.elapsed().as_secs_f64();
    lines.push(format!("Elapsed: {}", crate::paths::format_time(elapsed)));

    if let Some(error) = &save_error {
        tracker.fail(
            error,
            vec![
                ("tracks", json!(track_count)),
                ("edges", json!(edges)),
                ("analyzed", json!(analyzed)),
                ("failed", json!(failed)),
                ("loaded", json!(loaded)),
                ("skipped", json!(skipped)),
            ],
        );
    } else {
        tracker.finish(
            if cancel.load(Ordering::SeqCst) {
                "scan cancelled"
            } else {
                "analysis complete"
            },
            vec![
                ("tracks", json!(track_count)),
                ("edges", json!(edges)),
                ("analyzed", json!(analyzed)),
                ("failed", json!(failed)),
                ("loaded", json!(loaded)),
                ("skipped", json!(skipped)),
                ("phase", json!("done")),
            ],
        );
    }
    log_scan_summary(&paths.analysis_log, &lines);

    let _ = message_tx.send(WorkerMessage::Finished(ScanOutcome {
        summary: lines.join("\n"),
        removed_ids,
        tracks: track_count,
        edges,
        loaded,
        updated,
        analyzed,
        failed,
        error: save_error,
    }));
}
