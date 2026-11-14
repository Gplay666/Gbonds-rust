use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::graph::{Edge, Graph};
use crate::model::{Library, Track};
use crate::paths::{display_folder, normalize_folder, path_is_within, utc_now};
use crate::util::{append_log, atomic_write_json};

pub const LIBRARY_FORMAT: &str = "tui-music-library";
pub const LIBRARY_VERSION: u32 = 1;
pub const FOLDER_FORMAT: &str = "tui-music-folders";
pub const FOLDER_VERSION: u32 = 1;
pub const PLAYLIST_FORMAT: &str = "tui-music-playlists";
pub const PLAYLIST_VERSION: u32 = 1;
pub const STATE_FORMAT: &str = "tui-music-analysis-state";
pub const STATE_VERSION: u32 = 1;

#[derive(Clone, Debug, Default)]
pub struct LoadReport {
    pub found: bool,
    pub loaded: usize,
    pub edges: usize,
    pub skipped: usize,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct SaveReport {
    pub edges: usize,
}

#[derive(Debug, Deserialize)]
struct LibraryFile {
    #[serde(default)]
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    saved_at: Option<String>,
    #[serde(default)]
    tracks: Vec<Value>,
    #[serde(default)]
    graph: GraphFile,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Default, Deserialize)]
struct GraphFile {
    #[serde(default)]
    directed: bool,
    #[serde(default)]
    edges: Vec<Edge>,
}

#[derive(Serialize)]
struct GraphOut<'a> {
    directed: bool,
    edges: &'a [Edge],
}

#[derive(Serialize)]
struct LibraryOut<'a> {
    format: &'static str,
    version: u32,
    saved_at: String,
    tracks: &'a [Track],
    graph: GraphOut<'a>,
    #[serde(flatten)]
    extra: &'a Map<String, Value>,
}

pub fn load_library(path: &Path) -> (Library, LoadReport) {
    let mut report = LoadReport {
        found: path.exists(),
        ..Default::default()
    };
    if !report.found {
        return (Library::new(), report);
    }

    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            report.error = Some(format!("Cannot read library cache: {error}"));
            return (Library::new(), report);
        }
    };
    let file: LibraryFile = match serde_json::from_str(&text) {
        Ok(file) => file,
        Err(error) => {
            report.error = Some(format!("Cannot parse library cache: {error}"));
            return (Library::new(), report);
        }
    };
    if file.format != LIBRARY_FORMAT {
        report.error = Some("Library cache has an unknown format.".to_string());
        return (Library::new(), report);
    }
    if file.version != LIBRARY_VERSION {
        report.error = Some(format!(
            "Library cache version {} is not supported (expected {}).",
            file.version, LIBRARY_VERSION
        ));
        return (Library::new(), report);
    }

    let mut library = Library::new();
    library.graph = Graph::new(file.graph.directed);
    library.saved_at = file.saved_at;
    library.extra = file.extra;

    for value in file.tracks {
        match serde_json::from_value::<Track>(value) {
            Ok(track) if !track.id.is_empty() => {
                if library.add_track(track) {
                    report.loaded += 1;
                } else {
                    report.skipped += 1;
                }
            }
            _ => report.skipped += 1,
        }
    }

    for edge in file.graph.edges {
        if edge.from == edge.to || !library.contains(&edge.from) || !library.contains(&edge.to) {
            report.skipped += 1;
            continue;
        }
        library.graph.add_edge(edge.from, edge.to, edge.weight);
        report.edges += 1;
    }
    library.bump();
    (library, report)
}

pub fn save_library(library: &Library, path: &Path) -> anyhow::Result<SaveReport> {
    let output = LibraryOut {
        format: LIBRARY_FORMAT,
        version: LIBRARY_VERSION,
        saved_at: utc_now(),
        tracks: &library.tracks,
        graph: GraphOut {
            directed: library.graph.directed,
            edges: &library.graph.edges,
        },
        extra: &library.extra,
    };
    atomic_write_json(path, &output)?;
    Ok(SaveReport {
        edges: library.graph.edges.len(),
    })
}

#[derive(Clone, Debug)]
pub struct FolderStore {
    pub path: PathBuf,
    pub folders: Vec<String>,
    pub watch_enabled: bool,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FolderFile {
    #[serde(default)]
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default = "default_true")]
    watch: bool,
    #[serde(default)]
    folders: Vec<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Serialize)]
struct FolderOut<'a> {
    format: &'static str,
    version: u32,
    saved_at: String,
    watch: bool,
    folders: &'a [String],
}

impl FolderStore {
    pub fn load(path: PathBuf) -> Self {
        let mut store = Self {
            path,
            folders: Vec::new(),
            watch_enabled: true,
            error: None,
        };
        let text = match fs::read_to_string(&store.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return store,
            Err(error) => {
                store.error = Some(format!("Cannot read folder list: {error}"));
                return store;
            }
        };
        let file: FolderFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(error) => {
                store.error = Some(format!("Cannot parse folder list: {error}"));
                return store;
            }
        };
        if file.format != FOLDER_FORMAT || file.version != FOLDER_VERSION {
            store.error = Some("Folder list has an unknown format or version.".to_string());
            return store;
        }
        store.watch_enabled = file.watch;
        let mut seen = std::collections::HashSet::new();
        for folder in file.folders {
            if folder.trim().is_empty() {
                continue;
            }
            let folder = normalize_folder(&folder);
            if seen.insert(folder.clone()) {
                store.folders.push(folder);
            }
        }
        store
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let output = FolderOut {
            format: FOLDER_FORMAT,
            version: FOLDER_VERSION,
            saved_at: utc_now(),
            watch: self.watch_enabled,
            folders: &self.folders,
        };
        atomic_write_json(&self.path, &output)
    }

    pub fn add(&mut self, path: &str) -> Result<String, String> {
        let normalized = normalize_folder(path);
        if !Path::new(&normalized).is_dir() {
            return Err(format!("Folder not found: {path}"));
        }
        if self.folders.contains(&normalized) {
            return Err("This folder is already in the library.".to_string());
        }
        for existing in &self.folders {
            if path_is_within(&normalized, existing) {
                return Err(format!(
                    "This folder is already inside {}.",
                    display_folder(existing)
                ));
            }
            if path_is_within(existing, &normalized) {
                return Err(format!(
                    "The added folder already contains {}.",
                    display_folder(existing)
                ));
            }
        }
        self.folders.push(normalized.clone());
        self.save().map_err(|error| error.to_string())?;
        Ok(normalized)
    }

    pub fn remove(&mut self, path: &str) -> bool {
        let normalized = normalize_folder(path);
        let before = self.folders.len();
        self.folders.retain(|folder| folder != &normalized);
        if before == self.folders.len() {
            return false;
        }
        let _ = self.save();
        true
    }

    pub fn set_watch_enabled(&mut self, enabled: bool) {
        if self.watch_enabled == enabled {
            return;
        }
        self.watch_enabled = enabled;
        let _ = self.save();
    }
}

#[derive(Clone, Debug)]
pub struct PlaylistStore {
    pub path: PathBuf,
    pub playlists: HashMap<String, Vec<String>>,
    pub order: Vec<String>,
    pub revision: u64,
}

#[derive(Debug, Deserialize)]
struct PlaylistFile {
    #[serde(default)]
    format: String,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    playlists: Map<String, Value>,
}

#[derive(Serialize)]
struct PlaylistOut<'a> {
    format: &'static str,
    version: u32,
    saved_at: String,
    playlists: &'a Map<String, Value>,
}

impl PlaylistStore {
    pub fn load(path: PathBuf) -> Self {
        let mut store = Self {
            path,
            playlists: HashMap::new(),
            order: Vec::new(),
            revision: 1,
        };
        let text = match fs::read_to_string(&store.path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return store,
            Err(_) => return store,
        };
        let file: PlaylistFile = match serde_json::from_str(&text) {
            Ok(file) => file,
            Err(_) => return store,
        };
        if file.format != PLAYLIST_FORMAT || file.version != PLAYLIST_VERSION {
            return store;
        }
        for (name, ids) in file.playlists {
            let name = name.trim().to_string();
            if name.is_empty() {
                continue;
            }
            let Some(ids) = ids.as_array() else {
                continue;
            };
            let ids: Vec<String> = ids
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect();
            store.playlists.insert(name.clone(), ids);
            store.order.push(name);
        }
        store
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let mut map = Map::new();
        for name in &self.order {
            if let Some(ids) = self.playlists.get(name) {
                map.insert(
                    name.clone(),
                    Value::Array(ids.iter().cloned().map(Value::String).collect()),
                );
            }
        }
        let output = PlaylistOut {
            format: PLAYLIST_FORMAT,
            version: PLAYLIST_VERSION,
            saved_at: utc_now(),
            playlists: &map,
        };
        atomic_write_json(&self.path, &output)
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn names(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn get(&self, name: &str) -> Vec<String> {
        self.playlists.get(name).cloned().unwrap_or_default()
    }

    pub fn count(&self, name: &str) -> usize {
        self.playlists.get(name).map(Vec::len).unwrap_or(0)
    }

    pub fn total_items(&self) -> usize {
        self.playlists.values().map(Vec::len).sum()
    }

    pub fn create(&mut self, name: &str) -> Result<String, String> {
        let clean = name.trim();
        if clean.is_empty() {
            return Err("Playlist name cannot be empty.".to_string());
        }
        if clean.chars().count() > 80 {
            return Err("Playlist name is too long.".to_string());
        }
        if self.playlists.contains_key(clean) {
            return Err("A playlist with this name already exists.".to_string());
        }
        self.playlists.insert(clean.to_string(), Vec::new());
        self.order.push(clean.to_string());
        self.save().map_err(|error| error.to_string())?;
        self.bump();
        Ok(clean.to_string())
    }

    pub fn delete(&mut self, name: &str) -> bool {
        if self.playlists.remove(name).is_none() {
            return false;
        }
        self.order.retain(|item| item != name);
        let _ = self.save();
        self.bump();
        true
    }

    pub fn rename(&mut self, old: &str, new: &str) -> Result<String, String> {
        let clean = new.trim();
        if !self.playlists.contains_key(old) {
            return Err("Playlist not found.".to_string());
        }
        if clean.is_empty() {
            return Err("Playlist name cannot be empty.".to_string());
        }
        if clean != old && self.playlists.contains_key(clean) {
            return Err("A playlist with this name already exists.".to_string());
        }
        if let Some(ids) = self.playlists.remove(old) {
            self.playlists.insert(clean.to_string(), ids);
        }
        for item in &mut self.order {
            if item == old {
                *item = clean.to_string();
            }
        }
        self.save().map_err(|error| error.to_string())?;
        self.bump();
        Ok(clean.to_string())
    }

    pub fn add(&mut self, name: &str, ids: &[String]) -> Result<usize, String> {
        let playlist = self
            .playlists
            .get_mut(name)
            .ok_or_else(|| "Playlist not found.".to_string())?;
        playlist.extend_from_slice(ids);
        let count = ids.len();
        self.save().map_err(|error| error.to_string())?;
        self.bump();
        Ok(count)
    }

    pub fn remove_at(&mut self, name: &str, index: usize) -> bool {
        let Some(playlist) = self.playlists.get_mut(name) else {
            return false;
        };
        if index >= playlist.len() {
            return false;
        }
        playlist.remove(index);
        let _ = self.save();
        self.bump();
        true
    }

    pub fn move_track(&mut self, name: &str, from: usize, to: usize) -> bool {
        let Some(playlist) = self.playlists.get_mut(name) else {
            return false;
        };
        if from >= playlist.len() || to >= playlist.len() || from == to {
            return false;
        }
        let id = playlist.remove(from);
        playlist.insert(to, id);
        let _ = self.save();
        self.bump();
        true
    }

    pub fn clear(&mut self, name: &str) -> bool {
        let Some(playlist) = self.playlists.get_mut(name) else {
            return false;
        };
        playlist.clear();
        let _ = self.save();
        self.bump();
        true
    }

    pub fn remove_track_everywhere(&mut self, track_id: &str) -> usize {
        let mut removed = 0;
        for ids in self.playlists.values_mut() {
            let before = ids.len();
            ids.retain(|id| id != track_id);
            removed += before - ids.len();
        }
        if removed > 0 {
            let _ = self.save();
            self.bump();
        }
        removed
    }

    pub fn prune_missing(&mut self, library: &crate::model::Library) -> usize {
        let mut removed = 0;
        for ids in self.playlists.values_mut() {
            let before = ids.len();
            ids.retain(|id| library.contains(id));
            removed += before - ids.len();
        }
        if removed > 0 {
            let _ = self.save();
            self.bump();
        }
        removed
    }
}

/// Live analysis state for `--status` and for observing a scan from another
/// terminal. The on-disk shape matches the Python app.
pub struct Tracker {
    path: PathBuf,
    status: String,
    stage: String,
    processed: usize,
    total: usize,
    message: String,
    current: String,
    current_name: String,
    started_at: String,
    updated_at: String,
    started: Instant,
    last_write: Instant,
    extras: Map<String, Value>,
}

impl Tracker {
    pub fn start(path: PathBuf, stage: &str, total: usize, message: &str) -> Self {
        let now = Instant::now();
        Self {
            path,
            status: "running".to_string(),
            stage: stage.to_string(),
            processed: 0,
            total,
            message: message.to_string(),
            current: String::new(),
            current_name: String::new(),
            started_at: utc_now(),
            updated_at: utc_now(),
            started: now,
            last_write: now - Duration::from_secs(10),
            extras: Map::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        stage: Option<&str>,
        processed: Option<usize>,
        total: Option<usize>,
        current: Option<&str>,
        current_name: Option<&str>,
        message: Option<&str>,
        force: bool,
    ) {
        if let Some(stage) = stage {
            self.stage = stage.to_string();
        }
        if let Some(processed) = processed {
            self.processed = processed;
        }
        if let Some(total) = total {
            self.total = total;
        }
        if let Some(current) = current {
            self.current = current.to_string();
            if current_name.is_none() {
                self.current_name = Path::new(current)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
            }
        }
        if let Some(current_name) = current_name {
            self.current_name = current_name.to_string();
        }
        if let Some(message) = message {
            self.message = message.to_string();
        }
        self.status = "running".to_string();
        self.updated_at = utc_now();
        self.write(force);
    }

    pub fn set_extra(&mut self, key: &str, value: Value) {
        self.extras.insert(key.to_string(), value);
    }

    pub fn finish(&mut self, message: &str, extras: Vec<(&str, Value)>) {
        self.status = "done".to_string();
        self.stage = "done".to_string();
        self.message = message.to_string();
        self.current.clear();
        self.current_name.clear();
        for (key, value) in extras {
            self.extras.insert(key.to_string(), value);
        }
        self.updated_at = utc_now();
        self.extras.insert(
            "finished_at".to_string(),
            Value::String(self.updated_at.clone()),
        );
        self.write(true);
    }

    pub fn fail(&mut self, error: &str, extras: Vec<(&str, Value)>) {
        self.status = "error".to_string();
        self.stage = "error".to_string();
        self.message = error.to_string();
        for (key, value) in extras {
            self.extras.insert(key.to_string(), value);
        }
        self.updated_at = utc_now();
        self.extras
            .insert("error".to_string(), Value::String(error.to_string()));
        self.extras.insert(
            "finished_at".to_string(),
            Value::String(self.updated_at.clone()),
        );
        self.write(true);
    }

    fn write(&mut self, force: bool) {
        let now = Instant::now();
        if !force && now.duration_since(self.last_write) < Duration::from_millis(150) {
            return;
        }
        self.last_write = now;
        let elapsed = (now.duration_since(self.started).as_secs_f64() * 10.0).round() / 10.0;
        let mut map = Map::new();
        map.insert("format".into(), json!(STATE_FORMAT));
        map.insert("version".into(), json!(STATE_VERSION));
        map.insert("pid".into(), json!(std::process::id()));
        map.insert("status".into(), json!(self.status));
        map.insert("stage".into(), json!(self.stage));
        map.insert("processed".into(), json!(self.processed));
        map.insert("total".into(), json!(self.total));
        map.insert("message".into(), json!(self.message));
        map.insert("current".into(), json!(self.current));
        map.insert("current_name".into(), json!(self.current_name));
        map.insert("started_at".into(), json!(self.started_at));
        map.insert("updated_at".into(), json!(self.updated_at));
        map.insert("elapsed_seconds".into(), json!(elapsed));
        for (key, value) in &self.extras {
            map.insert(key.clone(), value.clone());
        }
        let _ = atomic_write_json(&self.path, &Value::Object(map));
    }
}

pub fn read_analysis_state(path: &Path) -> Option<Map<String, Value>> {
    let text = fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    let map = value.as_object()?.clone();
    if map.get("format").and_then(Value::as_str) != Some(STATE_FORMAT) {
        return None;
    }
    Some(map)
}

pub fn format_analysis_state(state: Option<&Map<String, Value>>) -> String {
    let Some(state) = state else {
        return "Analysis state not found.\n".to_string();
    };
    let status = state
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let stage = state.get("stage").and_then(Value::as_str).unwrap_or("idle");
    let processed = state.get("processed").and_then(Value::as_u64).unwrap_or(0);
    let total = state.get("total").and_then(Value::as_u64).unwrap_or(0);
    let elapsed = state
        .get("elapsed_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let mut lines = vec![format!("Stage: {stage}")];
    if status != "done" {
        if total > 0 {
            lines.push(format!(
                "Progress: {processed}/{total} ({:.0}%)",
                processed as f64 * 100.0 / total as f64
            ));
        } else {
            lines.push(format!("Progress: {processed}"));
        }
    }
    lines.push(format!("Time: {}", crate::paths::format_time(elapsed)));
    if let Some(name) = state
        .get("current_name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            state
                .get("current")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
        })
    {
        lines.push(format!("Now: {name}"));
    }
    if let Some(message) = state
        .get("message")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        lines.push(format!("Message: {message}"));
    }
    if status == "done" {
        if let Some(tracks) = state.get("tracks") {
            lines.push(format!("Tracks: {tracks}"));
        }
        if let Some(edges) = state.get("edges") {
            lines.push(format!("Edges: {edges}"));
        }
    }
    if status == "error" {
        let error = state
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| state.get("message").and_then(Value::as_str))
            .unwrap_or("unknown error");
        lines.push(format!("Error: {error}"));
    }
    for key in ["pid", "updated_at"] {
        if let Some(value) = state.get(key) {
            let label = if key == "pid" { "PID" } else { "Updated" };
            lines.push(format!("{label}: {value}"));
        }
    }
    lines.join("\n") + "\n"
}

pub fn log_scan_summary(path: &Path, lines: &[String]) {
    let _ = append_log(path, &format!("\n{}\n", lines.join("\n")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Library, Track};
    use serde_json::json;

    fn scratch_path(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gbonds-{name}-{}-{unique}.json",
            std::process::id()
        ))
    }

    #[test]
    fn library_roundtrip_preserves_unknown_fields() {
        let path = scratch_path("roundtrip");
        let mut library = Library::new();
        let mut first = Track {
            id: "1".to_string(),
            title: "First".to_string(),
            artist: "Artist".to_string(),
            filepath: Some("/music/first.mp3".to_string()),
            ..Default::default()
        };
        first
            .extra
            .insert("custom_track".to_string(), json!({"x": 1}));
        let second = Track {
            id: "2".to_string(),
            title: "Second".to_string(),
            filepath: Some("/music/second.mp3".to_string()),
            ..Default::default()
        };
        assert!(library.add_track(first));
        assert!(library.add_track(second));
        library
            .graph
            .add_edge("1".to_string(), "2".to_string(), 0.75);
        library
            .extra
            .insert("custom_library".to_string(), json!("kept"));

        let report = save_library(&library, &path).expect("save");
        assert_eq!(report.edges, 1);

        let raw = fs::read_to_string(&path).expect("read back");
        assert!(raw.contains("custom_library"));
        assert!(raw.contains("custom_track"));

        let (loaded, report) = load_library(&path);
        assert!(report.error.is_none());
        assert_eq!(report.loaded, 2);
        assert_eq!(report.edges, 1);
        assert_eq!(loaded.extra.get("custom_library"), Some(&json!("kept")));
        assert_eq!(
            loaded
                .find("1")
                .and_then(|track| track.extra.get("custom_track")),
            Some(&json!({"x": 1}))
        );
        assert_eq!(loaded.graph.get_edge_weight("1", "2"), Some(0.75));
        assert_eq!(loaded.graph.get_edge_weight("2", "1"), Some(0.75));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn library_rejects_unknown_format() {
        let path = scratch_path("format");
        fs::write(
            &path,
            r#"{"format":"something-else","version":1,"tracks":[],"graph":{"edges":[]}}"#,
        )
        .expect("write fixture");
        let (library, report) = load_library(&path);
        assert!(library.tracks.is_empty());
        assert!(report.error.as_deref().unwrap_or("").contains("format"));
        let _ = fs::remove_file(&path);
    }
}
