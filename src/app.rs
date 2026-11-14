//! Application state and input handling.
//!
//! The UI thread owns this state. Library, folder, playlist and analysis work
//! happens on background threads; `App` only exchanges short messages with
//! them and never blocks on ffmpeg or file scanning.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde_json::Map;

use crate::model::{normalize_text, Library, Track};
use crate::paths::{display_folder, format_time, path_is_within, AppPaths};
use crate::player::{Player, PlayerError};
use crate::rng::Rng;
use crate::similarity::{radio_track_ids, recommendations};
use crate::store::{self, FolderStore, PlaylistStore};
use crate::theme::{Theme, ThemeMode};
use crate::worker::{ProgressInfo, ScanOutcome, WorkerCommand, WorkerHandle, WorkerMessage};

pub const APP_NAME: &str = "gbonds";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const RADIO_LIMIT: usize = 30;
const PAGE_STEP: usize = 12;
const SIDEBAR_MIN_WIDTH: u16 = 84;
/// Rows moved by one mouse-wheel notch.
pub const WHEEL_STEP: isize = 3;
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(450);

// ---------------------------------------------------------------------------
// Views and navigation
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Library,
    ForYou,
    Queue,
    Playlists,
}

impl View {
    pub const ALL: [View; 4] = [View::Library, View::ForYou, View::Queue, View::Playlists];

    pub fn label(self) -> &'static str {
        match self {
            View::Library => "Library",
            View::ForYou => "For You",
            View::Queue => "Queue",
            View::Playlists => "Playlists",
        }
    }

    pub fn number(self) -> usize {
        match self {
            View::Library => 1,
            View::ForYou => 2,
            View::Queue => 3,
            View::Playlists => 4,
        }
    }

    pub fn from_digit(number: char) -> Option<Self> {
        match number {
            '1' => Some(View::Library),
            '2' => Some(View::ForYou),
            '3' => Some(View::Queue),
            '4' => Some(View::Playlists),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            View::Library => View::ForYou,
            View::ForYou => View::Queue,
            View::Queue => View::Playlists,
            View::Playlists => View::Library,
        }
    }

    pub fn previous(self) -> Self {
        match self {
            View::Library => View::Playlists,
            View::ForYou => View::Library,
            View::Queue => View::ForYou,
            View::Playlists => View::Queue,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Content,
}

/// The visible track table registered by the UI for mouse hit-testing.
#[derive(Clone, Copy, Debug)]
pub struct TableHitbox {
    pub view: View,
    pub pane: PlaylistPane,
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub offset: usize,
    pub rows: usize,
}

impl TableHitbox {
    pub fn contains(&self, column: u16, row: u16) -> bool {
        column >= self.x
            && column < self.x.saturating_add(self.width)
            && row >= self.y
            && row < self.y.saturating_add(self.height)
    }

    pub fn track_index(&self, row: u16) -> Option<usize> {
        if row < self.y || row >= self.y.saturating_add(self.height) {
            return None;
        }
        let index = self.offset + (row - self.y) as usize;
        (index < self.rows).then_some(index)
    }
}

/// Keeps `selected` inside the viewport, moving the viewport minimally.
fn ensure_visible(selected: usize, offset: &mut usize, len: usize, visible_rows: usize) {
    if len == 0 {
        *offset = 0;
        return;
    }
    let visible = visible_rows.max(1).min(len);
    let max_offset = len - visible;
    if selected < *offset {
        *offset = selected;
    } else if selected >= *offset + visible {
        *offset = selected + 1 - visible;
    }
    *offset = (*offset).min(max_offset);
}

/// Scrolls the viewport. The selection is only moved when it would leave the
/// viewport, which keeps mouse-wheel scrolling from acting like arrow keys.
fn scroll_viewport(
    selected: &mut usize,
    offset: &mut usize,
    len: usize,
    visible_rows: usize,
    delta: isize,
) {
    if len == 0 {
        *selected = 0;
        *offset = 0;
        return;
    }
    let visible = visible_rows.max(1).min(len);
    let max_offset = len - visible;
    let target = (*offset as isize + delta).clamp(0, max_offset as isize);
    *offset = target as usize;
    if *selected < *offset {
        *selected = *offset;
    } else if *selected >= *offset + visible {
        *selected = *offset + visible - 1;
    }
    *selected = (*selected).min(len - 1);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaylistPane {
    List,
    Tracks,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidebarEntry {
    View(View),
    Playlist(String),
    Folder(String),
}

#[derive(Clone, Copy, Debug)]
pub struct SidebarState {
    pub index: usize,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self { index: 0 }
    }
}

// ---------------------------------------------------------------------------
// Sorting and list state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SortMode {
    Id,
    Title,
    Artist,
    Album,
    Year,
    Bpm,
    Rating,
    Duration,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Id => "id",
            SortMode::Title => "title",
            SortMode::Artist => "artist",
            SortMode::Album => "album",
            SortMode::Year => "year",
            SortMode::Bpm => "bpm",
            SortMode::Rating => "rating",
            SortMode::Duration => "duration",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "id" | "number" | "num" => Some(SortMode::Id),
            "title" | "name" => Some(SortMode::Title),
            "artist" => Some(SortMode::Artist),
            "album" => Some(SortMode::Album),
            "year" | "date" => Some(SortMode::Year),
            "bpm" | "tempo" => Some(SortMode::Bpm),
            "rating" | "stars" => Some(SortMode::Rating),
            "duration" | "time" => Some(SortMode::Duration),
            _ => None,
        }
    }

    pub fn next(self) -> Self {
        match self {
            SortMode::Id => SortMode::Title,
            SortMode::Title => SortMode::Artist,
            SortMode::Artist => SortMode::Album,
            SortMode::Album => SortMode::Year,
            SortMode::Year => SortMode::Bpm,
            SortMode::Bpm => SortMode::Rating,
            SortMode::Rating => SortMode::Duration,
            SortMode::Duration => SortMode::Id,
        }
    }
}

pub struct LibraryState {
    pub selected: usize,
    pub offset: usize,
    pub visible_rows: usize,
    pub ids: Vec<String>,
    pub revision: u64,
    pub query: String,
    pub sort: SortMode,
    dirty: bool,
}

impl LibraryState {
    fn new() -> Self {
        Self {
            selected: 0,
            offset: 0,
            visible_rows: 16,
            ids: Vec::new(),
            revision: 0,
            query: String::new(),
            sort: SortMode::Id,
            dirty: true,
        }
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn ensure(&mut self, library: &Library) {
        if !self.dirty && self.revision == library.revision {
            return;
        }
        let terms: Vec<String> = normalize_text(&self.query)
            .split_whitespace()
            .map(ToString::to_string)
            .collect();
        let mut tracks: Vec<&Track> = library
            .tracks
            .iter()
            .filter(|track| {
                if terms.is_empty() {
                    return true;
                }
                let haystack = normalize_text(&format!(
                    "{} {} {} {} {}",
                    track.title,
                    track.artist,
                    track.album.as_deref().unwrap_or(""),
                    track.display_genre(),
                    track.filepath.as_deref().unwrap_or("")
                ));
                terms.iter().all(|term| haystack.contains(term))
            })
            .collect();
        tracks.sort_by(|left, right| compare_tracks(left, right, self.sort));
        self.ids = tracks.iter().map(|track| track.id.clone()).collect();
        self.selected = clamp_index(self.selected, self.ids.len());
        ensure_visible(
            self.selected,
            &mut self.offset,
            self.ids.len(),
            self.visible_rows,
        );
        self.revision = library.revision;
        self.dirty = false;
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.ids.get(self.selected).map(String::as_str)
    }

    pub fn move_by(&mut self, delta: isize) {
        let length = self.ids.len();
        if length == 0 {
            self.selected = 0;
            return;
        }
        let target = (self.selected as isize + delta).clamp(0, length as isize - 1);
        self.selected = target as usize;
    }
}

fn clamp_index(index: usize, length: usize) -> usize {
    if length == 0 {
        0
    } else {
        index.min(length - 1)
    }
}

fn compare_text(left: &str, right: &str) -> std::cmp::Ordering {
    left.to_lowercase().cmp(&right.to_lowercase())
}

fn compare_tracks(left: &Track, right: &Track, sort: SortMode) -> std::cmp::Ordering {
    let ordering = match sort {
        SortMode::Id => left.sort_key().cmp(&right.sort_key()),
        SortMode::Title => compare_text(&left.title, &right.title),
        SortMode::Artist => compare_text(&left.artist, &right.artist)
            .then_with(|| compare_text(&left.title, &right.title)),
        SortMode::Album => compare_text(
            left.album.as_deref().unwrap_or(""),
            right.album.as_deref().unwrap_or(""),
        )
        .then_with(|| {
            left.track_number
                .unwrap_or(0)
                .cmp(&right.track_number.unwrap_or(0))
        })
        .then_with(|| compare_text(&left.title, &right.title)),
        SortMode::Year => left.year.unwrap_or(0).cmp(&right.year.unwrap_or(0)),
        SortMode::Bpm => metric(left.bpm())
            .partial_cmp(&metric(right.bpm()))
            .unwrap_or(std::cmp::Ordering::Equal),
        SortMode::Rating => right.rating.cmp(&left.rating),
        SortMode::Duration => left
            .duration_seconds()
            .partial_cmp(&right.duration_seconds())
            .unwrap_or(std::cmp::Ordering::Equal),
    };
    ordering.then_with(|| left.sort_key().cmp(&right.sort_key()))
}

fn metric(value: f64) -> f64 {
    if value > 0.0 && value.is_finite() {
        value
    } else {
        f64::INFINITY
    }
}

pub struct RecommendState {
    pub seed_id: Option<String>,
    pub ids: Vec<String>,
    pub selected: usize,
    pub offset: usize,
    pub visible_rows: usize,
    pub revision: u64,
    dirty: bool,
}

impl RecommendState {
    fn new() -> Self {
        Self {
            seed_id: None,
            ids: Vec::new(),
            selected: 0,
            offset: 0,
            visible_rows: 16,
            revision: 0,
            dirty: true,
        }
    }

    pub fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub fn set_seed(&mut self, seed: &str) {
        self.seed_id = Some(seed.to_string());
        self.selected = 0;
        self.offset = 0;
        self.dirty = true;
    }

    pub fn ensure(&mut self, library: &Library) {
        if self.seed_id.is_none() {
            self.ids.clear();
            self.selected = 0;
            self.offset = 0;
            return;
        }
        if !self.dirty && self.revision == library.revision {
            return;
        }
        let seed = self.seed_id.clone().unwrap_or_default();
        if !library.contains(&seed) {
            self.seed_id = None;
            self.ids.clear();
            self.selected = 0;
            self.offset = 0;
            return;
        }
        self.ids = recommendations(library, &seed, 120);
        self.selected = clamp_index(self.selected, self.ids.len());
        ensure_visible(
            self.selected,
            &mut self.offset,
            self.ids.len(),
            self.visible_rows,
        );
        self.revision = library.revision;
        self.dirty = false;
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.ids.get(self.selected).map(String::as_str)
    }

    pub fn move_by(&mut self, delta: isize) {
        let length = self.ids.len();
        if length == 0 {
            self.selected = 0;
            return;
        }
        let target = (self.selected as isize + delta).clamp(0, length as isize - 1);
        self.selected = target as usize;
    }
}

pub struct ContextState {
    pub selected_id: Option<String>,
    pub recs: Vec<String>,
    revision: u64,
}

impl ContextState {
    fn new() -> Self {
        Self {
            selected_id: None,
            recs: Vec::new(),
            revision: 0,
        }
    }

    fn ensure(&mut self, library: &Library, selected: Option<&str>) {
        if self.selected_id.as_deref() == selected && self.revision == library.revision {
            return;
        }
        self.selected_id = selected.map(ToString::to_string);
        self.recs = match selected {
            Some(id) => recommendations(library, id, 6),
            None => Vec::new(),
        };
        self.revision = library.revision;
    }
}

pub struct PlaylistsState {
    pub playlist_index: usize,
    pub track_index: usize,
    pub track_offset: usize,
    pub track_visible_rows: usize,
    pub ids: Vec<String>,
    loaded_name: Option<String>,
    revision: u64,
    library_revision: u64,
}

impl PlaylistsState {
    fn new() -> Self {
        Self {
            playlist_index: 0,
            track_index: 0,
            track_offset: 0,
            track_visible_rows: 16,
            ids: Vec::new(),
            loaded_name: None,
            revision: 0,
            library_revision: 0,
        }
    }

    pub fn selected_id(&self) -> Option<&str> {
        self.ids.get(self.track_index).map(String::as_str)
    }

    pub fn ensure(&mut self, playlists: &PlaylistStore, library: &Library) {
        let names = playlists.names();
        if names.is_empty() {
            self.playlist_index = 0;
            self.ids.clear();
            self.track_index = 0;
            self.track_offset = 0;
            self.loaded_name = None;
            return;
        }
        self.playlist_index = clamp_index(self.playlist_index, names.len());
        let name = names[self.playlist_index].clone();
        if self.loaded_name.as_deref() != Some(name.as_str())
            || self.revision != playlists.revision
            || self.library_revision != library.revision
        {
            self.ids = playlists.get(&name);
            self.track_index = clamp_index(self.track_index, self.ids.len());
            ensure_visible(
                self.track_index,
                &mut self.track_offset,
                self.ids.len(),
                self.track_visible_rows,
            );
            self.loaded_name = Some(name);
            self.revision = playlists.revision;
            self.library_revision = library.revision;
        }
    }

    pub fn select_playlist(&mut self, index: usize) {
        if self.playlist_index != index {
            self.playlist_index = index;
            self.loaded_name = None;
            self.track_offset = 0;
        }
    }

    pub fn move_track(&mut self, delta: isize) {
        let length = self.ids.len();
        if length == 0 {
            self.track_index = 0;
            return;
        }
        let target = (self.track_index as isize + delta).clamp(0, length as isize - 1);
        self.track_index = target as usize;
    }
}

// ---------------------------------------------------------------------------
// Prompts, overlays and transient state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusKind {
    Info,
    Working,
    Success,
    Error,
}

#[derive(Clone, Debug)]
pub enum PromptKind {
    Search,
    Command,
    AddFolder,
    NewPlaylist { add_ids: Vec<String> },
    RenamePlaylist { old: String },
}

#[derive(Clone)]
pub struct PromptState {
    pub kind: PromptKind,
    pub title: String,
    pub value: String,
    pub cursor: usize,
    pub original: String,
    pub history_index: Option<usize>,
}

impl PromptState {
    fn new(kind: PromptKind) -> Self {
        let (title, value) = match &kind {
            PromptKind::Search => ("Search".to_string(), String::new()),
            PromptKind::Command => ("Command".to_string(), String::new()),
            PromptKind::AddFolder => ("Add music folder".to_string(), String::new()),
            PromptKind::NewPlaylist { .. } => ("New playlist name".to_string(), String::new()),
            PromptKind::RenamePlaylist { old } => ("Rename playlist".to_string(), old.clone()),
        };
        let cursor = value.len();
        Self {
            kind,
            title,
            value: value.clone(),
            cursor,
            original: value,
            history_index: None,
        }
    }

    pub fn insert(&mut self, text: &str) {
        let text = text.replace(['\n', '\r'], " ");
        self.value.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.value[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.value.replace_range(previous..self.cursor, "");
        self.cursor = previous;
    }

    pub fn delete(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let next = self.value[self.cursor..]
            .char_indices()
            .nth(1)
            .map(|(index, _)| self.cursor + index)
            .unwrap_or(self.value.len());
        self.value.replace_range(self.cursor..next, "");
    }

    pub fn left(&mut self) {
        if let Some((index, _)) = self.value[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    pub fn right(&mut self) {
        if let Some((index, character)) = self.value[self.cursor..].char_indices().next() {
            self.cursor += index + character.len_utf8();
        }
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.value.len();
    }

    pub fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let prefix = &self.value[..self.cursor];
        let trimmed = prefix.trim_end();
        let cut = trimmed
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.value.replace_range(cut..self.cursor, "");
        self.cursor = cut;
    }

    pub fn clear_before_cursor(&mut self) {
        self.value.replace_range(..self.cursor, "");
        self.cursor = 0;
    }
}

pub struct HelpOverlay {
    pub offset: usize,
}

pub struct FoldersOverlay {
    pub selected: usize,
}

pub struct InfoOverlay {
    pub id: String,
    pub offset: usize,
}

pub struct AnalysisOverlay {
    pub state: Option<Map<String, serde_json::Value>>,
    pub log: Vec<String>,
    pub offset: usize,
}

pub struct PlaylistPickerOverlay {
    pub ids: Vec<String>,
    pub selected: usize,
}

#[derive(Clone, Debug)]
pub enum ConfirmAction {
    RemoveFolder(String),
    DeletePlaylist(String),
    RemoveTrack(String),
    ForceAnalyze,
    ClearQueue,
}

pub struct ConfirmOverlay {
    pub question: String,
    pub action: ConfirmAction,
}

pub enum Overlay {
    Help(HelpOverlay),
    Folders(FoldersOverlay),
    Info(InfoOverlay),
    Analysis(AnalysisOverlay),
    PlaylistPicker(PlaylistPickerOverlay),
    Confirm(ConfirmOverlay),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingKey {
    G,
    Z,
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    pub paths: AppPaths,
    pub library: Arc<RwLock<Library>>,
    pub player: Player,
    pub folders: FolderStore,
    pub playlists: PlaylistStore,
    pub worker: WorkerHandle,

    pub view: View,
    pub focus: Focus,
    pub playlist_pane: PlaylistPane,
    pub sidebar: SidebarState,
    pub library_state: LibraryState,
    pub recommendations: RecommendState,
    pub context: ContextState,
    pub playlists_state: PlaylistsState,
    pub queue_selected: usize,
    pub queue_offset: usize,
    pub queue_visible_rows: usize,
    pub table_hitbox: Option<TableHitbox>,
    last_click: Option<(View, usize, Instant)>,

    pub prompt: Option<PromptState>,
    pub overlay: Option<Overlay>,
    pending: Option<PendingKey>,
    command_history: Vec<String>,

    pub status: String,
    pub status_kind: StatusKind,
    pub scan: Option<ProgressInfo>,
    pending_scan: Option<bool>,
    pub should_quit: bool,

    pub theme_mode: ThemeMode,
    pub theme: Theme,

    pub width: u16,
    pub height: u16,
    pub rng: Rng,
    saved_volume: i32,
    last_player_error: Option<String>,
}

impl App {
    pub fn new(paths: AppPaths, theme_mode: ThemeMode) -> Self {
        let (library, report) = store::load_library(&paths.library);
        let library = Arc::new(RwLock::new(library));
        let mut folders = FolderStore::load(paths.folders.clone());
        let playlists = PlaylistStore::load(paths.playlists.clone());
        let player = Player::new(Arc::clone(&library), 80);
        let worker = WorkerHandle::spawn(paths.clone(), Arc::clone(&library));

        let mut app = Self {
            paths,
            library,
            player,
            folders: FolderStore {
                path: folders.path.clone(),
                folders: Vec::new(),
                watch_enabled: folders.watch_enabled,
                error: None,
            },
            playlists,
            worker,
            view: View::Library,
            focus: Focus::Content,
            playlist_pane: PlaylistPane::List,
            sidebar: SidebarState::default(),
            library_state: LibraryState::new(),
            recommendations: RecommendState::new(),
            context: ContextState::new(),
            playlists_state: PlaylistsState::new(),
            queue_selected: 0,
            queue_offset: 0,
            queue_visible_rows: 16,
            table_hitbox: None,
            last_click: None,
            prompt: None,
            overlay: None,
            pending: None,
            command_history: Vec::new(),
            status: String::new(),
            status_kind: StatusKind::Info,
            scan: None,
            pending_scan: None,
            should_quit: false,
            theme_mode,
            theme: Theme::for_mode(theme_mode),
            width: 120,
            height: 40,
            rng: Rng::from_time(),
            saved_volume: 80,
            last_player_error: None,
        };

        // Move the real folder list in after constructing the skeleton above.
        app.folders.folders = std::mem::take(&mut folders.folders);
        app.folders.error = folders.error.take();

        let auto_scan = !app.folders.folders.is_empty()
            && app
                .library
                .read()
                .map(|library| library.tracks.is_empty())
                .unwrap_or(false);

        app.worker.send(WorkerCommand::SetWatch {
            enabled: app.folders.watch_enabled,
            roots: app.folders.folders.clone(),
        });

        if let Some(error) = report.error {
            app.set_status(StatusKind::Error, error);
        } else if report.loaded > 0 {
            app.set_status(
                StatusKind::Info,
                format!(
                    "Loaded {} tracks · {} links from the library cache",
                    report.loaded, report.edges
                ),
            );
        } else if app.folders.folders.is_empty() {
            app.set_status(
                StatusKind::Info,
                "No music folders yet — press o to add one",
            );
        }

        app.prune_playlists();

        if auto_scan {
            app.start_scan(false);
        }
        app.refresh();
        app
    }

    pub fn shutdown(&mut self) {
        self.worker.shutdown();
        self.player.shutdown();
    }

    pub fn sidebar_visible(&self) -> bool {
        self.width >= SIDEBAR_MIN_WIDTH
    }

    pub fn total_tracks(&self) -> usize {
        self.library
            .read()
            .map(|library| library.tracks.len())
            .unwrap_or(0)
    }

    pub fn edge_count(&self) -> usize {
        self.library
            .read()
            .map(|library| library.graph.edge_count())
            .unwrap_or(0)
    }

    pub fn current_playlist_name(&self) -> Option<String> {
        self.playlists
            .names()
            .get(self.playlists_state.playlist_index)
            .cloned()
    }

    pub fn sidebar_entries(&self) -> Vec<SidebarEntry> {
        let mut entries: Vec<SidebarEntry> = View::ALL
            .iter()
            .map(|view| SidebarEntry::View(*view))
            .collect();
        entries.extend(
            self.playlists
                .names()
                .into_iter()
                .map(SidebarEntry::Playlist),
        );
        entries.extend(
            self.folders
                .folders
                .iter()
                .cloned()
                .map(SidebarEntry::Folder),
        );
        entries
    }

    fn ensure_sidebar(&mut self) {
        let length = self.sidebar_entries().len();
        self.sidebar.index = clamp_index(self.sidebar.index, length);
    }

    fn sidebar_select_view(&mut self, view: View) {
        if let Some(index) = self
            .sidebar_entries()
            .iter()
            .position(|entry| matches!(entry, SidebarEntry::View(item) if *item == view))
        {
            self.sidebar.index = index;
        }
    }

    /// Keeps cached views in sync with the library and stores.
    pub fn refresh(&mut self) {
        let library = Arc::clone(&self.library);
        let guard = match library.read() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        self.library_state.ensure(&guard);
        self.recommendations.ensure(&guard);
        let context_target = self.context_target_from(&guard);
        self.context.ensure(&guard, context_target.as_deref());
        self.playlists_state.ensure(&self.playlists, &guard);
        drop(guard);

        self.ensure_sidebar();
        if !self.player.queue.is_empty() {
            self.queue_selected = clamp_index(self.queue_selected, self.player.queue.len());
        } else {
            self.queue_selected = 0;
        }
    }

    fn context_target_from(&self, library: &Library) -> Option<String> {
        match self.view {
            View::Library => self.library_state.selected_id().map(ToString::to_string),
            View::ForYou => self.recommendations.selected_id().map(ToString::to_string),
            View::Queue => self
                .player
                .queue
                .get(self.queue_selected)
                .cloned()
                .filter(|id| library.contains(id)),
            View::Playlists => self.playlists_state.selected_id().map(ToString::to_string),
        }
    }

    pub fn context_target(&self) -> Option<String> {
        let library = self.library.read().ok()?;
        self.context_target_from(&library)
    }

    fn selected_any_id(&self) -> Option<String> {
        match self.view {
            View::Library => self.library_state.selected_id().map(ToString::to_string),
            View::ForYou => self.recommendations.selected_id().map(ToString::to_string),
            View::Queue => self.player.queue.get(self.queue_selected).cloned(),
            View::Playlists => self.playlists_state.selected_id().map(ToString::to_string),
        }
    }

    pub fn current_track_id(&self) -> Option<String> {
        self.player.current_id.clone()
    }

    // -----------------------------------------------------------------------
    // Status helpers
    // -----------------------------------------------------------------------

    pub fn set_status(&mut self, kind: StatusKind, text: impl Into<String>) {
        self.status_kind = kind;
        self.status = text.into();
    }

    fn info(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Info, text);
    }

    fn success(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Success, text);
    }

    fn warn(&mut self, text: impl Into<String>) {
        self.set_status(StatusKind::Error, text);
    }

    pub fn mode_label(&self) -> &'static str {
        match self.status_kind {
            StatusKind::Info => "NORMAL",
            StatusKind::Working => "WORKING",
            StatusKind::Success => "OK",
            StatusKind::Error => "ERROR",
        }
    }

    fn player_error(&mut self, error: PlayerError) {
        self.warn(error.0);
    }

    fn label_for(&self, id: &str) -> String {
        self.library
            .read()
            .ok()
            .and_then(|library| library.find(id).map(Track::title_artist))
            .unwrap_or_else(|| id.to_string())
    }

    // -----------------------------------------------------------------------
    // Views and focus
    // -----------------------------------------------------------------------

    pub fn switch_view(&mut self, view: View) {
        self.view = view;
        self.focus = Focus::Content;
        match view {
            View::ForYou => {
                if self.recommendations.seed_id.is_none() {
                    let seed = self
                        .current_track_id()
                        .or_else(|| self.library_state.selected_id().map(ToString::to_string));
                    if let Some(seed) = seed {
                        self.recommendations.set_seed(&seed);
                    }
                }
            }
            View::Queue => {
                self.queue_selected = if self.player.queue.is_empty() {
                    0
                } else {
                    clamp_index(
                        self.player.queue_index.max(0) as usize,
                        self.player.queue.len(),
                    )
                };
            }
            View::Playlists => {
                self.playlist_pane = PlaylistPane::List;
            }
            View::Library => {}
        }
        self.refresh();
    }

    fn switch_view_from_shortcut(&mut self, view: View) {
        self.switch_view(view);
        self.sidebar_select_view(view);
    }

    fn focus_left(&mut self) {
        match self.view {
            View::Playlists if self.focus == Focus::Content => {
                if self.playlist_pane == PlaylistPane::Tracks {
                    self.playlist_pane = PlaylistPane::List;
                } else if self.sidebar_visible() {
                    self.focus = Focus::Sidebar;
                }
            }
            _ => {
                if self.sidebar_visible() {
                    self.focus = Focus::Sidebar;
                }
            }
        }
    }

    fn focus_right(&mut self) {
        match self.focus {
            Focus::Sidebar => {
                self.focus = Focus::Content;
            }
            Focus::Content => {
                if self.view == View::Playlists && self.playlist_pane == PlaylistPane::List {
                    self.playlist_pane = PlaylistPane::Tracks;
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Key handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return;
        }
        if let Some(prompt) = self.prompt.take() {
            self.handle_prompt_key(prompt, key);
            return;
        }
        if let Some(overlay) = self.overlay.take() {
            self.handle_overlay_key(overlay, key);
            return;
        }
        if let Some(pending) = self.pending.take() {
            match pending {
                PendingKey::G => {
                    if key.code == KeyCode::Char('g')
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        self.move_selection(isize::MIN / 2);
                    } else {
                        self.handle_normal_key(key);
                    }
                }
                PendingKey::Z => {
                    if matches!(
                        key.code,
                        KeyCode::Char('Z') | KeyCode::Char('Q') | KeyCode::Char('q')
                    ) {
                        self.should_quit = true;
                    }
                }
            }
            return;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if key.code == KeyCode::Esc {
            if !self.library_state.query.is_empty() {
                self.library_state.query.clear();
                self.library_state.invalidate();
                self.success("Search cleared");
            } else {
                self.focus = Focus::Content;
            }
            return;
        }

        match key.code {
            KeyCode::Char(':') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_prompt(PromptKind::Command);
                return;
            }
            KeyCode::Char('/') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                let mut prompt = PromptState::new(PromptKind::Search);
                prompt.value = self.library_state.query.clone();
                prompt.cursor = prompt.value.len();
                prompt.original = prompt.value.clone();
                self.prompt = Some(prompt);
                return;
            }
            KeyCode::Char('?') => {
                self.open_help();
                return;
            }
            KeyCode::Char('q') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.should_quit = true;
                return;
            }
            KeyCode::Char('Z') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.pending = Some(PendingKey::Z);
                return;
            }
            KeyCode::Char('g') if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.pending = Some(PendingKey::G);
                return;
            }
            KeyCode::Tab => {
                self.switch_view_from_shortcut(self.view.next());
                return;
            }
            KeyCode::BackTab => {
                self.switch_view_from_shortcut(self.view.previous());
                return;
            }
            KeyCode::Char(digit) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(view) = View::from_digit(digit) {
                    self.switch_view_from_shortcut(view);
                    return;
                }
            }
            _ => {}
        }

        self.handle_normal_key(key);
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('h') => self.focus_left(),
            KeyCode::Char('l') => self.focus_right(),
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(PAGE_STEP as isize)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_selection(-(PAGE_STEP as isize))
            }
            KeyCode::PageDown => self.move_selection(PAGE_STEP as isize),
            KeyCode::PageUp => self.move_selection(-(PAGE_STEP as isize)),
            KeyCode::Home => self.move_selection(isize::MIN / 2),
            KeyCode::End | KeyCode::Char('G') => self.move_selection(isize::MAX / 2),
            KeyCode::Enter => self.activate_selected(),
            KeyCode::Char(' ') => self.toggle_playback(),
            KeyCode::Char('>') => self.next_track(),
            KeyCode::Char('<') => self.previous_track(),
            KeyCode::Char(']') => self.seek_relative(5.0),
            KeyCode::Char('[') => self.seek_relative(-5.0),
            KeyCode::Char('+') | KeyCode::Char('=') => self.change_volume(5),
            KeyCode::Char('-') => self.change_volume(-5),
            KeyCode::Char('M') => self.mute_toggle(),
            KeyCode::Char('s') => self.toggle_shuffle(),
            KeyCode::Char('S') => self.cycle_repeat(),
            KeyCode::Char('e') => self.enqueue_selected(),
            KeyCode::Char('E') => self.enqueue_all_visible(),
            KeyCode::Char('r') => {
                if self.view == View::Playlists && self.playlist_pane == PlaylistPane::List {
                    self.rename_playlist_prompt();
                } else {
                    self.action_recommendations();
                }
            }
            KeyCode::Char('n') => self.search_next(true),
            KeyCode::Char('N') => self.search_next(false),
            KeyCode::Char('R') => self.action_radio(),
            KeyCode::Char('a') => {
                if self.view == View::Playlists && self.playlist_pane == PlaylistPane::List {
                    self.start_prompt(PromptKind::NewPlaylist {
                        add_ids: Vec::new(),
                    });
                } else {
                    self.add_to_playlist(false);
                }
            }
            KeyCode::Char('A') => self.add_to_playlist(true),
            KeyCode::Char('x') => self.remove_selected_soft(),
            KeyCode::Char('D') => self.remove_track_prompt(),
            KeyCode::Char('J') => self.move_item(1),
            KeyCode::Char('K') => self.move_item(-1),
            KeyCode::Char('d') => {
                if self.view == View::Playlists && self.playlist_pane == PlaylistPane::List {
                    self.delete_playlist_prompt();
                }
            }
            KeyCode::Char('o') => self.start_prompt(PromptKind::AddFolder),
            KeyCode::Char('O') => self.open_folders(),
            KeyCode::Char('w') => self.toggle_watch(),
            KeyCode::Char('t') => self.cycle_theme(),
            KeyCode::Char('i') => self.open_info(),
            KeyCode::Char('I') => self.open_analysis(),
            KeyCode::Char('F') => self.cycle_sort(),
            KeyCode::Char('f') => self.jump_to_current(),
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {}
            KeyCode::Char('C') => self.clear_queue_prompt(),
            KeyCode::Char('g') => self.move_selection(isize::MIN / 2),
            _ => {}
        }
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.prompt.is_some() || self.overlay.is_some() {
            return;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => self.scroll_active(-WHEEL_STEP),
            MouseEventKind::ScrollDown => self.scroll_active(WHEEL_STEP),
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_track_click(mouse.column, mouse.row)
            }
            _ => {}
        }
    }

    fn handle_track_click(&mut self, column: u16, row: u16) {
        let Some(hitbox) = self.table_hitbox else {
            return;
        };
        if !hitbox.contains(column, row) || hitbox.view != self.view {
            return;
        }
        let Some(index) = hitbox.track_index(row) else {
            return;
        };

        self.focus = Focus::Content;
        match hitbox.view {
            View::Library => self.library_state.selected = index,
            View::ForYou => self.recommendations.selected = index,
            View::Queue => self.queue_selected = index,
            View::Playlists => {
                self.playlist_pane = hitbox.pane;
                self.playlists_state.track_index = index;
            }
        }

        let now = Instant::now();
        let double_click = self.last_click.is_some_and(|(view, clicked, at)| {
            view == hitbox.view && clicked == index && now.duration_since(at) <= DOUBLE_CLICK_WINDOW
        });
        self.last_click = if double_click {
            None
        } else {
            Some((hitbox.view, index, now))
        };
        self.refresh();
        if double_click {
            self.activate_selected();
        }
    }

    /// Mouse wheel always scrolls the visible list. The selection only moves
    /// when it would otherwise leave the viewport.
    fn scroll_active(&mut self, delta: isize) {
        self.refresh();
        match self.view {
            View::Library => {
                let state = &mut self.library_state;
                scroll_viewport(
                    &mut state.selected,
                    &mut state.offset,
                    state.ids.len(),
                    state.visible_rows,
                    delta,
                );
            }
            View::ForYou => {
                let state = &mut self.recommendations;
                scroll_viewport(
                    &mut state.selected,
                    &mut state.offset,
                    state.ids.len(),
                    state.visible_rows,
                    delta,
                );
            }
            View::Queue => {
                scroll_viewport(
                    &mut self.queue_selected,
                    &mut self.queue_offset,
                    self.player.queue.len(),
                    self.queue_visible_rows,
                    delta,
                );
            }
            View::Playlists => {
                let state = &mut self.playlists_state;
                scroll_viewport(
                    &mut state.track_index,
                    &mut state.track_offset,
                    state.ids.len(),
                    state.track_visible_rows,
                    delta,
                );
            }
        }
        self.refresh();
    }

    /// Keeps programmatic selection jumps (for example `f`, auto-advance or
    /// search matches) inside the viewport.
    pub fn ensure_active_visible(&mut self) {
        match self.view {
            View::Library => {
                let state = &mut self.library_state;
                ensure_visible(
                    state.selected,
                    &mut state.offset,
                    state.ids.len(),
                    state.visible_rows,
                );
            }
            View::ForYou => {
                let state = &mut self.recommendations;
                ensure_visible(
                    state.selected,
                    &mut state.offset,
                    state.ids.len(),
                    state.visible_rows,
                );
            }
            View::Queue => ensure_visible(
                self.queue_selected,
                &mut self.queue_offset,
                self.player.queue.len(),
                self.queue_visible_rows,
            ),
            View::Playlists => {
                let state = &mut self.playlists_state;
                ensure_visible(
                    state.track_index,
                    &mut state.track_offset,
                    state.ids.len(),
                    state.track_visible_rows,
                );
            }
        }
    }

    fn move_selection(&mut self, delta: isize) {
        self.refresh();
        if self.focus == Focus::Sidebar {
            let length = self.sidebar_entries().len();
            if length == 0 {
                return;
            }
            let target = (self.sidebar.index as isize + delta).clamp(0, length as isize - 1);
            self.sidebar.index = target as usize;
            return;
        }
        match self.view {
            View::Library => self.library_state.move_by(delta),
            View::ForYou => self.recommendations.move_by(delta),
            View::Queue => {
                let length = self.player.queue.len();
                if length == 0 {
                    self.queue_selected = 0;
                    return;
                }
                let target = (self.queue_selected as isize + delta).clamp(0, length as isize - 1);
                self.queue_selected = target as usize;
            }
            View::Playlists => {
                if self.playlist_pane == PlaylistPane::List {
                    let length = self.playlists.names().len();
                    if length == 0 {
                        self.playlists_state.playlist_index = 0;
                        return;
                    }
                    let target = (self.playlists_state.playlist_index as isize + delta)
                        .clamp(0, length as isize - 1);
                    if target as usize != self.playlists_state.playlist_index {
                        self.playlists_state.select_playlist(target as usize);
                        self.playlists_state.track_index = 0;
                    }
                } else {
                    self.playlists_state.move_track(delta);
                }
            }
        }
        self.ensure_active_visible();
        self.refresh();
    }

    fn activate_selected(&mut self) {
        self.refresh();
        if self.focus == Focus::Sidebar {
            self.activate_sidebar();
            return;
        }
        match self.view {
            View::Library => self.play_library_selection(),
            View::ForYou => self.play_recommendation_selection(),
            View::Queue => self.play_queue_selection(),
            View::Playlists => {
                if self.playlist_pane == PlaylistPane::List {
                    self.play_playlist_from(0);
                } else {
                    self.play_playlist_from(self.playlists_state.track_index);
                }
            }
        }
    }

    fn activate_sidebar(&mut self) {
        let entry = self.sidebar_entries().get(self.sidebar.index).cloned();
        match entry {
            Some(SidebarEntry::View(view)) => self.switch_view(view),
            Some(SidebarEntry::Playlist(name)) => {
                if let Some(index) = self.playlists.names().iter().position(|item| item == &name) {
                    self.playlists_state.select_playlist(index);
                }
                self.view = View::Playlists;
                self.focus = Focus::Content;
                self.playlist_pane = PlaylistPane::Tracks;
                self.refresh();
                self.info(format!("Playlist: {name}"));
            }
            Some(SidebarEntry::Folder(folder)) => {
                let index = self
                    .folders
                    .folders
                    .iter()
                    .position(|item| item == &folder)
                    .unwrap_or(0);
                self.open_folders_at(index);
            }
            None => {}
        }
    }

    // -----------------------------------------------------------------------
    // Prompt handling
    // -----------------------------------------------------------------------

    fn start_prompt(&mut self, kind: PromptKind) {
        self.prompt = Some(PromptState::new(kind));
    }

    fn handle_prompt_key(&mut self, mut prompt: PromptState, key: KeyEvent) {
        let mut keep = true;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                if matches!(prompt.kind, PromptKind::Search) {
                    self.library_state.query = prompt.original.clone();
                    self.library_state.invalidate();
                }
                keep = false;
            }
            KeyCode::Enter => {
                // `submit_prompt` keeps a copy only when the prompt has to
                // stay open; pass a clone so the local prompt is still usable.
                keep = !self.submit_prompt(prompt.clone());
            }
            KeyCode::Backspace => {
                prompt.backspace();
            }
            KeyCode::Delete => prompt.delete(),
            KeyCode::Left => prompt.left(),
            KeyCode::Right => prompt.right(),
            KeyCode::Home => prompt.home(),
            KeyCode::End => prompt.end(),
            KeyCode::Up if matches!(prompt.kind, PromptKind::Command) => {
                if !self.command_history.is_empty() {
                    let index = prompt
                        .history_index
                        .map(|index| index.saturating_sub(1))
                        .unwrap_or(self.command_history.len() - 1);
                    prompt.history_index = Some(index);
                    if let Some(command) = self.command_history.get(index) {
                        prompt.value = command.clone();
                        prompt.cursor = prompt.value.len();
                    }
                }
            }
            KeyCode::Down if matches!(prompt.kind, PromptKind::Command) => {
                if let Some(index) = prompt.history_index {
                    if index + 1 < self.command_history.len() {
                        let index = index + 1;
                        prompt.history_index = Some(index);
                        if let Some(command) = self.command_history.get(index) {
                            prompt.value = command.clone();
                            prompt.cursor = prompt.value.len();
                        }
                    } else {
                        prompt.history_index = None;
                        prompt.value.clear();
                        prompt.cursor = 0;
                    }
                }
            }
            KeyCode::Char('u') if control => prompt.clear_before_cursor(),
            KeyCode::Char('w') if control => prompt.delete_word(),
            KeyCode::Char('a') if control => prompt.home(),
            KeyCode::Char('e') if control => prompt.end(),
            KeyCode::Char(character) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                prompt.insert(&character.to_string());
            }
            KeyCode::Tab => {
                // Keep the prompt open; Tab is intentionally a no-op so a
                // mistyped command never switches views.
            }
            _ => {}
        }

        if keep && self.prompt.is_none() {
            if matches!(prompt.kind, PromptKind::Search) {
                self.library_state.query = prompt.value.clone();
                self.library_state.invalidate();
            }
            self.prompt = Some(prompt);
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if let Some(prompt) = self.prompt.as_mut() {
            prompt.insert(text);
            if matches!(prompt.kind, PromptKind::Search) {
                self.library_state.query = prompt.value.clone();
                self.library_state.invalidate();
            }
        }
    }

    /// Returns true when the prompt should close.
    fn submit_prompt(&mut self, prompt: PromptState) -> bool {
        let value = prompt.value.trim().to_string();
        match prompt.kind {
            PromptKind::Search => {
                self.library_state.query = prompt.value.clone();
                self.library_state.invalidate();
                self.refresh();
                self.focus = Focus::Content;
                true
            }
            PromptKind::Command => {
                if !value.is_empty() {
                    self.command_history.push(value.clone());
                }
                self.execute_command(&value);
                true
            }
            PromptKind::AddFolder => {
                if value.is_empty() {
                    self.prompt = Some(prompt);
                    return false;
                }
                match self.folders.add(&value) {
                    Ok(folder) => {
                        self.success(format!("Added folder {}", display_folder(&folder)));
                        self.sync_watch();
                        self.start_scan(false);
                        true
                    }
                    Err(error) => {
                        self.warn(error);
                        self.prompt = Some(prompt);
                        false
                    }
                }
            }
            PromptKind::NewPlaylist { ref add_ids } => {
                if value.is_empty() {
                    self.prompt = Some(prompt);
                    return false;
                }
                match self.playlists.create(&value) {
                    Ok(name) => {
                        if !add_ids.is_empty() {
                            let _ = self.playlists.add(&name, add_ids);
                        }
                        if let Some(index) =
                            self.playlists.names().iter().position(|item| item == &name)
                        {
                            self.playlists_state.select_playlist(index);
                        }
                        self.view = View::Playlists;
                        self.focus = Focus::Content;
                        self.playlist_pane = PlaylistPane::List;
                        self.success(format!(
                            "Playlist “{name}” created{}",
                            if add_ids.is_empty() {
                                String::new()
                            } else {
                                format!(" with {} track(s)", add_ids.len())
                            }
                        ));
                        self.refresh();
                        true
                    }
                    Err(error) => {
                        self.warn(error);
                        self.prompt = Some(prompt);
                        false
                    }
                }
            }
            PromptKind::RenamePlaylist { ref old } => {
                if value.is_empty() {
                    self.prompt = Some(prompt);
                    return false;
                }
                match self.playlists.rename(old, &value) {
                    Ok(name) => {
                        if let Some(index) =
                            self.playlists.names().iter().position(|item| item == &name)
                        {
                            self.playlists_state.select_playlist(index);
                        }
                        self.playlists_state.loaded_name = None;
                        self.success(format!("Renamed playlist to “{name}”"));
                        self.refresh();
                        true
                    }
                    Err(error) => {
                        self.warn(error);
                        self.prompt = Some(prompt);
                        false
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Overlays
    // -----------------------------------------------------------------------

    pub fn open_help(&mut self) {
        self.overlay = Some(Overlay::Help(HelpOverlay { offset: 0 }));
    }

    fn open_folders(&mut self) {
        self.open_folders_at(0);
    }

    fn open_folders_at(&mut self, index: usize) {
        let length = self.folders.folders.len();
        self.overlay = Some(Overlay::Folders(FoldersOverlay {
            selected: clamp_index(index, length),
        }));
    }

    fn open_info(&mut self) {
        let Some(id) = self.selected_any_id() else {
            self.warn("Select a track first");
            return;
        };
        self.overlay = Some(Overlay::Info(InfoOverlay { id, offset: 0 }));
    }

    fn open_analysis(&mut self) {
        self.overlay = Some(Overlay::Analysis(self.analysis_overlay()));
    }

    fn analysis_overlay(&self) -> AnalysisOverlay {
        AnalysisOverlay {
            state: store::read_analysis_state(&self.paths.analysis_state),
            log: tail_log(&self.paths.analysis_log, 50),
            offset: 0,
        }
    }

    fn open_confirm(&mut self, question: impl Into<String>, action: ConfirmAction) {
        self.overlay = Some(Overlay::Confirm(ConfirmOverlay {
            question: question.into(),
            action,
        }));
    }

    fn open_playlist_picker(&mut self, ids: Vec<String>) {
        if ids.is_empty() {
            self.warn("Nothing to add");
            return;
        }
        if self.playlists.names().is_empty() {
            self.start_prompt(PromptKind::NewPlaylist { add_ids: ids });
            return;
        }
        self.overlay = Some(Overlay::PlaylistPicker(PlaylistPickerOverlay {
            ids,
            selected: 0,
        }));
    }

    fn handle_overlay_key(&mut self, mut overlay: Overlay, key: KeyEvent) {
        let mut keep = true;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut overlay {
            Overlay::Help(state) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') => keep = false,
                KeyCode::Char('j') | KeyCode::Down => state.offset += 1,
                KeyCode::Char('k') | KeyCode::Up => state.offset = state.offset.saturating_sub(1),
                KeyCode::Char('d') if control => state.offset += PAGE_STEP,
                KeyCode::Char('u') if control => {
                    state.offset = state.offset.saturating_sub(PAGE_STEP)
                }
                KeyCode::Char('g') => state.offset = 0,
                KeyCode::Char('G') | KeyCode::End => state.offset = usize::MAX / 2,
                KeyCode::PageDown => state.offset += PAGE_STEP,
                KeyCode::PageUp => state.offset = state.offset.saturating_sub(PAGE_STEP),
                _ => {}
            },
            Overlay::Folders(state) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => keep = false,
                KeyCode::Char('j') | KeyCode::Down => {
                    state.selected = clamp_index(state.selected + 1, self.folders.folders.len());
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    state.selected = state.selected.saturating_sub(1);
                }
                KeyCode::Char('g') => state.selected = 0,
                KeyCode::Char('G') | KeyCode::End => {
                    state.selected = self.folders.folders.len().saturating_sub(1);
                }
                KeyCode::Char('a') | KeyCode::Char('o') => {
                    self.start_prompt(PromptKind::AddFolder);
                    keep = false;
                }
                KeyCode::Char('d') | KeyCode::Delete => {
                    if let Some(folder) = self.folders.folders.get(state.selected).cloned() {
                        self.open_confirm(
                            format!(
                                "Remove {} from the library? Tracks from this folder will be removed from the cache. Files stay on disk.",
                                display_folder(&folder)
                            ),
                            ConfirmAction::RemoveFolder(folder),
                        );
                        keep = false;
                    }
                }
                KeyCode::Char('w') => {
                    self.toggle_watch();
                }
                KeyCode::Char('r') => {
                    self.start_scan(false);
                    keep = false;
                }
                KeyCode::Enter => {
                    if let Some(folder) = self.folders.folders.get(state.selected).cloned() {
                        self.start_scan_roots(vec![folder], false, true);
                        keep = false;
                    }
                }
                _ => {}
            },
            Overlay::Info(state) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('i') => keep = false,
                KeyCode::Char('j') | KeyCode::Down => state.offset += 1,
                KeyCode::Char('k') | KeyCode::Up => state.offset = state.offset.saturating_sub(1),
                KeyCode::Char('d') if control => state.offset += PAGE_STEP,
                KeyCode::Char('u') if control => {
                    state.offset = state.offset.saturating_sub(PAGE_STEP)
                }
                KeyCode::Char('g') => state.offset = 0,
                KeyCode::Char('G') | KeyCode::End => state.offset = usize::MAX / 2,
                _ => {}
            },
            Overlay::Analysis(state) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('I') => keep = false,
                KeyCode::Char('j') | KeyCode::Down => state.offset += 1,
                KeyCode::Char('k') | KeyCode::Up => state.offset = state.offset.saturating_sub(1),
                KeyCode::Char('d') if control => state.offset += PAGE_STEP,
                KeyCode::Char('u') if control => {
                    state.offset = state.offset.saturating_sub(PAGE_STEP)
                }
                KeyCode::Char('g') => state.offset = 0,
                KeyCode::Char('G') | KeyCode::End => state.offset = usize::MAX / 2,
                KeyCode::Char('r') => {
                    *state = self.analysis_overlay();
                }
                _ => {}
            },
            Overlay::PlaylistPicker(state) => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => keep = false,
                KeyCode::Char('j') | KeyCode::Down => {
                    state.selected = clamp_index(state.selected + 1, self.playlists.names().len());
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    state.selected = state.selected.saturating_sub(1);
                }
                KeyCode::Char('g') => state.selected = 0,
                KeyCode::Char('G') | KeyCode::End => {
                    state.selected = self.playlists.names().len().saturating_sub(1);
                }
                KeyCode::Enter => {
                    if let Some(name) = self.playlists.names().get(state.selected).cloned() {
                        match self.playlists.add(&name, &state.ids) {
                            Ok(count) => {
                                self.success(format!("Added {count} track(s) to “{name}”"));
                                keep = false;
                            }
                            Err(error) => self.warn(error),
                        }
                    }
                }
                KeyCode::Char('a') => {
                    let ids = state.ids.clone();
                    self.start_prompt(PromptKind::NewPlaylist { add_ids: ids });
                    keep = false;
                }
                _ => {}
            },
            Overlay::Confirm(state) => match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('Y') => {
                    let action = state.action.clone();
                    self.overlay = None;
                    self.run_confirm(action);
                    return;
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Char('q') => {
                    keep = false;
                }
                _ => {}
            },
        }
        if keep {
            self.overlay = Some(overlay);
        }
    }

    fn run_confirm(&mut self, action: ConfirmAction) {
        match action {
            ConfirmAction::RemoveFolder(folder) => self.remove_folder(&folder),
            ConfirmAction::DeletePlaylist(name) => self.delete_playlist(&name),
            ConfirmAction::RemoveTrack(id) => self.remove_track(&id),
            ConfirmAction::ForceAnalyze => self.start_scan(true),
            ConfirmAction::ClearQueue => {
                self.player.clear_queue();
                self.success("Queue cleared");
            }
        }
    }

    // -----------------------------------------------------------------------
    // Playback
    // -----------------------------------------------------------------------

    fn queue_ids_for(&self, ids: &[String], selected_id: &str) -> Option<(Vec<String>, usize)> {
        let library = self.library.read().ok()?;
        let selected_track = library.find(selected_id)?;
        if !selected_track.playable() {
            return None;
        }
        let queue: Vec<String> = ids
            .iter()
            .filter(|id| library.find(id).map(Track::playable).unwrap_or(false))
            .cloned()
            .collect();
        let start = queue.iter().position(|id| id == selected_id)?;
        Some((queue, start))
    }

    fn play_ids_from(&mut self, ids: &[String], selected_id: &str) {
        let Some((queue, start)) = self.queue_ids_for(ids, selected_id) else {
            self.warn(format!("Cannot play {}", self.label_for(selected_id)));
            return;
        };
        if let Err(error) = self.player.set_queue(&queue, false) {
            self.player_error(error);
            return;
        }
        match self.player.play_queue_index(start as i64) {
            Ok(()) => {
                let label = self.label_for(selected_id);
                self.success(format!("Playing {label}"));
                self.queue_selected = start;
            }
            Err(error) => self.player_error(error),
        }
    }

    fn play_library_selection(&mut self) {
        let Some(id) = self.library_state.selected_id().map(ToString::to_string) else {
            self.warn("The library is empty — press o to add a music folder");
            return;
        };
        let ids = self.library_state.ids.clone();
        self.play_ids_from(&ids, &id);
    }

    fn play_recommendation_selection(&mut self) {
        if let Some(id) = self.recommendations.selected_id().map(ToString::to_string) {
            let ids = self.recommendations.ids.clone();
            self.play_ids_from(&ids, &id);
        } else {
            self.warn("No recommendations yet — select a track and press r");
        }
    }

    fn play_queue_selection(&mut self) {
        if self.player.queue.is_empty() {
            self.warn("The queue is empty");
            return;
        }
        let index = clamp_index(self.queue_selected, self.player.queue.len());
        match self.player.play_queue_index(index as i64) {
            Ok(()) => {
                let label = self
                    .player
                    .queue
                    .get(index)
                    .map(|id| self.label_for(id))
                    .unwrap_or_default();
                self.success(format!("Playing {label}"));
            }
            Err(error) => self.player_error(error),
        }
    }

    fn play_playlist_from(&mut self, index: usize) {
        let Some(name) = self.current_playlist_name() else {
            self.warn("No playlist selected");
            return;
        };
        let ids = self.playlists.get(&name);
        if ids.is_empty() {
            self.warn(format!("Playlist “{name}” is empty"));
            return;
        }
        let index = clamp_index(index, ids.len());
        let selected = ids[index].clone();
        self.play_ids_from(&ids, &selected);
    }

    fn toggle_playback(&mut self) {
        if self.player.current_id.is_none() {
            match self.view {
                View::Queue => self.play_queue_selection(),
                View::Playlists => self.play_playlist_from(self.playlists_state.track_index),
                View::ForYou => self.play_recommendation_selection(),
                View::Library => self.play_library_selection(),
            }
            return;
        }
        match self.player.toggle() {
            Ok(()) => {}
            Err(error) => self.player_error(error),
        }
    }

    fn next_track(&mut self) {
        if !self.player.next() {
            self.info("End of queue");
        }
    }

    fn previous_track(&mut self) {
        if !self.player.previous() {
            self.info("Start of queue");
        }
    }

    fn seek_relative(&mut self, delta: f64) {
        match self.player.seek_relative(delta) {
            Ok(()) => self.info(format!(
                "{} / {}",
                format_time(self.player.position()),
                format_time(self.player.duration())
            )),
            Err(error) => self.player_error(error),
        }
    }

    fn change_volume(&mut self, delta: i32) {
        self.player.change_volume(delta);
        self.saved_volume = self.player.volume.max(1);
        self.info(format!("Volume {}%", self.player.volume));
    }

    fn mute_toggle(&mut self) {
        if self.player.volume > 0 {
            self.saved_volume = self.player.volume;
            self.player.set_volume(0);
            self.info("Muted");
        } else {
            let volume = if self.saved_volume > 0 {
                self.saved_volume
            } else {
                80
            };
            self.player.set_volume(volume);
            self.info(format!("Volume {volume}%"));
        }
    }

    fn toggle_shuffle(&mut self) {
        let shuffle = self.player.toggle_shuffle();
        self.info(format!("Shuffle {}", if shuffle { "on" } else { "off" }));
    }

    fn cycle_repeat(&mut self) {
        let repeat = self.player.cycle_repeat();
        self.info(format!("Repeat {repeat}"));
    }

    fn enqueue_selected(&mut self) {
        if self.view == View::Queue {
            self.info("That track is already in the queue");
            return;
        }
        let Some(id) = self.selected_any_id() else {
            self.warn("Select a track first");
            return;
        };
        self.player.enqueue_track(&id, false);
        self.success(format!("Queued {}", self.label_for(&id)));
    }

    fn enqueue_all_visible(&mut self) {
        let ids = match self.view {
            View::Library => self.library_state.ids.clone(),
            View::ForYou => self.recommendations.ids.clone(),
            View::Queue => Vec::new(),
            View::Playlists => self.playlists_state.ids.clone(),
        };
        if ids.is_empty() {
            self.warn("There is nothing to queue");
            return;
        }
        let playable: Vec<String> = {
            let library = self.library.read().ok();
            ids.into_iter()
                .filter(|id| {
                    library
                        .as_ref()
                        .and_then(|library| library.find(id))
                        .map(Track::playable)
                        .unwrap_or(false)
                })
                .collect()
        };
        let count = self.player.enqueue(&playable, false);
        self.success(format!("Queued {count} track(s)"));
    }

    fn jump_to_current(&mut self) {
        let Some(current) = self.current_track_id() else {
            self.info("Nothing is playing");
            return;
        };
        match self.view {
            View::Queue => {
                if let Some(index) = self.player.queue.iter().position(|id| id == &current) {
                    self.queue_selected = index;
                    self.focus = Focus::Content;
                }
            }
            View::Library => {
                if let Some(index) = self.library_state.ids.iter().position(|id| id == &current) {
                    self.library_state.selected = index;
                    self.focus = Focus::Content;
                } else {
                    self.info("The current track is hidden by the filter");
                }
            }
            View::ForYou => {
                if let Some(index) = self
                    .recommendations
                    .ids
                    .iter()
                    .position(|id| id == &current)
                {
                    self.recommendations.selected = index;
                } else {
                    self.recommendations.set_seed(&current);
                    self.refresh();
                }
            }
            View::Playlists => {
                if let Some(index) = self
                    .playlists_state
                    .ids
                    .iter()
                    .position(|id| id == &current)
                {
                    self.playlists_state.track_index = index;
                    self.playlist_pane = PlaylistPane::Tracks;
                }
            }
        }
        self.success(format!("Now playing: {}", self.label_for(&current)));
    }

    // -----------------------------------------------------------------------
    // Recommendations and radio
    // -----------------------------------------------------------------------

    fn action_recommendations(&mut self) {
        let seed = self
            .context_target()
            .or_else(|| self.current_track_id())
            .or_else(|| self.library_state.selected_id().map(ToString::to_string));
        let Some(seed) = seed else {
            self.warn("Select a track first");
            return;
        };
        if !self
            .library
            .read()
            .map(|library| {
                library
                    .find(&seed)
                    .map(Track::has_analysis)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
        {
            self.warn("This track has no audio analysis yet — run :scan or :analyze");
            return;
        }
        self.recommendations.set_seed(&seed);
        self.view = View::ForYou;
        self.focus = Focus::Content;
        self.refresh();
        let label = self.label_for(&seed);
        if self.recommendations.ids.is_empty() {
            self.warn(format!("No similar tracks found for {label}"));
        } else {
            self.success(format!("Similar to {label}"));
        }
    }

    fn action_radio(&mut self) {
        let seed = self
            .context_target()
            .or_else(|| self.current_track_id())
            .or_else(|| self.library_state.selected_id().map(ToString::to_string));
        let Some(seed) = seed else {
            self.warn("Select a track first");
            return;
        };
        let mut poisoned = false;
        let ids = match self.library.read() {
            Ok(library) => radio_track_ids(&library, &seed, RADIO_LIMIT, &mut self.rng),
            Err(_) => {
                poisoned = true;
                Vec::new()
            }
        };
        if poisoned {
            self.warn("Library lock is poisoned");
            return;
        }
        if ids.is_empty() {
            self.warn("Could not start radio for this track");
            return;
        }
        let queue: Vec<String> = {
            let library = self.library.read().ok();
            ids.into_iter()
                .filter(|id| {
                    library
                        .as_ref()
                        .and_then(|library| library.find(id))
                        .map(Track::playable)
                        .unwrap_or(false)
                })
                .collect()
        };
        if queue.is_empty() {
            self.warn("No playable tracks for radio");
            return;
        }
        if let Err(error) = self.player.set_queue(&queue, true) {
            self.player_error(error);
            return;
        }
        self.view = View::Queue;
        self.focus = Focus::Content;
        self.queue_selected = 0;
        let label = self.label_for(&seed);
        self.success(format!("Radio from {label} · {} tracks", queue.len()));
    }

    // -----------------------------------------------------------------------
    // Playlist actions
    // -----------------------------------------------------------------------

    fn add_to_playlist(&mut self, all: bool) {
        let ids: Vec<String> = if all {
            match self.view {
                View::Library => self.library_state.ids.clone(),
                View::ForYou => self.recommendations.ids.clone(),
                View::Queue => self.player.queue.clone(),
                View::Playlists => self.playlists_state.ids.clone(),
            }
        } else {
            self.selected_any_id().into_iter().collect()
        };
        if ids.is_empty() {
            self.warn("Select a track first");
            return;
        }
        self.open_playlist_picker(ids);
    }

    fn rename_playlist_prompt(&mut self) {
        let Some(name) = self.current_playlist_name() else {
            self.warn("No playlist selected");
            return;
        };
        self.start_prompt(PromptKind::RenamePlaylist { old: name });
    }

    fn delete_playlist_prompt(&mut self) {
        let Some(name) = self.current_playlist_name() else {
            self.warn("No playlist selected");
            return;
        };
        let count = self.playlists.count(&name);
        self.open_confirm(
            format!("Delete playlist “{name}” ({count} tracks)?"),
            ConfirmAction::DeletePlaylist(name),
        );
    }

    fn delete_playlist(&mut self, name: &str) {
        if !self.playlists.delete(name) {
            self.warn("Playlist not found");
            return;
        }
        self.playlists_state.select_playlist(
            self.playlists_state
                .playlist_index
                .min(self.playlists.names().len().saturating_sub(1)),
        );
        self.playlists_state.loaded_name = None;
        self.success(format!("Deleted playlist “{name}”"));
        self.refresh();
    }

    fn remove_selected_soft(&mut self) {
        match self.view {
            View::Queue => {
                if self.player.queue.is_empty() {
                    return;
                }
                let index = clamp_index(self.queue_selected, self.player.queue.len());
                match self.player.remove_queue_index(index as i64) {
                    Ok(()) => {
                        self.queue_selected = clamp_index(index, self.player.queue.len());
                        self.success("Removed from queue");
                    }
                    Err(error) => self.player_error(error),
                }
            }
            View::Playlists if self.playlist_pane == PlaylistPane::Tracks => {
                let Some(name) = self.current_playlist_name() else {
                    return;
                };
                let index = self.playlists_state.track_index;
                if self.playlists.remove_at(&name, index) {
                    self.playlists_state.loaded_name = None;
                    self.queue_selected = self.queue_selected.min(self.player.queue.len());
                    self.success("Removed track from playlist");
                    self.refresh();
                }
            }
            _ => self.info("Nothing to remove here"),
        }
    }

    fn move_item(&mut self, delta: isize) {
        match self.view {
            View::Queue => {
                if self.player.queue.is_empty() {
                    return;
                }
                let from = clamp_index(self.queue_selected, self.player.queue.len());
                let to = (from as isize + delta)
                    .clamp(0, self.player.queue.len().max(1) as isize - 1)
                    as usize;
                if from == to {
                    return;
                }
                if self.player.move_queue(from as i64, to as i64).is_ok() {
                    self.queue_selected = to;
                }
            }
            View::Playlists if self.playlist_pane == PlaylistPane::Tracks => {
                let Some(name) = self.current_playlist_name() else {
                    return;
                };
                let from = self.playlists_state.track_index;
                if self.playlists_state.ids.is_empty() {
                    return;
                }
                let to = (from as isize + delta)
                    .clamp(0, self.playlists_state.ids.len() as isize - 1)
                    as usize;
                if from != to && self.playlists.move_track(&name, from, to) {
                    self.playlists_state.track_index = to;
                    self.playlists_state.loaded_name = None;
                    self.refresh();
                }
            }
            _ => self.info("Reordering works in the queue and inside a playlist"),
        }
    }

    fn remove_track_prompt(&mut self) {
        let Some(id) = self.selected_any_id() else {
            self.warn("Select a track first");
            return;
        };
        let label = self.label_for(&id);
        self.open_confirm(
            format!("Remove “{label}” from the library cache? The file stays on disk."),
            ConfirmAction::RemoveTrack(id),
        );
    }

    fn remove_track(&mut self, id: &str) {
        let removed = self
            .library
            .write()
            .ok()
            .and_then(|mut library| library.remove_track(id))
            .is_some();
        if !removed {
            self.warn("Track not found");
            return;
        }
        self.playlists.remove_track_everywhere(id);
        self.player.remove_track(id);
        if let Err(error) = self.save_library_now() {
            self.warn(format!("Removed track, but could not save: {error}"));
        } else {
            self.success(format!("Removed {}", self.label_for(id)));
        }
        self.library_state.invalidate();
        self.recommendations.invalidate();
        self.context.revision = 0;
        self.playlists_state.loaded_name = None;
        self.refresh();
    }

    fn clear_queue_prompt(&mut self) {
        if self.player.queue.is_empty() {
            self.info("The queue is already empty");
            return;
        }
        self.open_confirm(
            format!("Clear the queue ({} tracks)?", self.player.queue.len()),
            ConfirmAction::ClearQueue,
        );
    }

    // -----------------------------------------------------------------------
    // Folder and scan actions
    // -----------------------------------------------------------------------

    fn sync_watch(&mut self) {
        self.worker.send(WorkerCommand::SetWatch {
            enabled: self.folders.watch_enabled,
            roots: self.folders.folders.clone(),
        });
    }

    fn toggle_watch(&mut self) {
        self.folders.set_watch_enabled(!self.folders.watch_enabled);
        self.sync_watch();
        self.info(format!(
            "Folder watcher {}",
            if self.folders.watch_enabled {
                "enabled"
            } else {
                "disabled"
            }
        ));
    }

    fn start_scan(&mut self, force_analysis: bool) {
        self.start_scan_roots(self.folders.folders.clone(), force_analysis, true);
    }

    fn start_scan_roots(&mut self, roots: Vec<String>, force_analysis: bool, prune: bool) {
        if roots.is_empty() {
            self.warn("No music folders configured — press o to add one");
            self.open_folders();
            return;
        }
        if self.scan.is_some() {
            self.pending_scan = Some(self.pending_scan.unwrap_or(false) || force_analysis);
            self.info("Scan queued — it will start after the current pass");
            return;
        }
        self.worker.send(WorkerCommand::Scan {
            roots,
            prune,
            force_analysis,
        });
        self.scan = Some(ProgressInfo {
            stage: "scan".to_string(),
            processed: 0,
            total: 0,
            message: if force_analysis {
                "preparing a full re-analysis".to_string()
            } else {
                "searching for audio files".to_string()
            },
            current: String::new(),
            current_name: String::new(),
            elapsed: 0.0,
            failed: 0,
            edges: 0,
            percent: 0.0,
        });
        self.set_status(
            StatusKind::Working,
            if force_analysis {
                "Scanning and re-analyzing every track…"
            } else {
                "Scanning music folders…"
            },
        );
    }

    fn remove_folder(&mut self, folder: &str) {
        if !self.folders.remove(folder) {
            self.warn("Folder not found");
            return;
        }
        let mut poisoned = false;
        let removed: Vec<String> = match self.library.write() {
            Ok(mut library) => {
                let ids: Vec<String> = library
                    .tracks
                    .iter()
                    .filter(|track| {
                        track
                            .filepath
                            .as_deref()
                            .map(|path| path_is_within(path, folder))
                            .unwrap_or(false)
                    })
                    .map(|track| track.id.clone())
                    .collect();
                for id in &ids {
                    library.remove_track(id);
                }
                ids
            }
            Err(_) => {
                poisoned = true;
                Vec::new()
            }
        };
        if poisoned {
            self.warn("Library lock is poisoned");
            return;
        }
        if !removed.is_empty() {
            for id in &removed {
                self.playlists.remove_track_everywhere(id);
                self.player.remove_track(id);
            }
            let _ = self.save_library_now();
        }
        self.sync_watch();
        self.library_state.invalidate();
        self.recommendations.invalidate();
        self.context.revision = 0;
        self.playlists_state.loaded_name = None;
        self.refresh();
        self.success(format!(
            "Removed {} and {} track(s)",
            display_folder(folder),
            removed.len()
        ));
        if !self.folders.folders.is_empty() {
            self.start_scan(false);
        }
    }

    fn cycle_sort(&mut self) {
        if self.view != View::Library {
            self.info("Sorting applies to the Library view");
            return;
        }
        self.library_state.sort = self.library_state.sort.next();
        self.library_state.invalidate();
        self.refresh();
        self.success(format!("Sorted by {}", self.library_state.sort.label()));
    }

    fn search_next(&mut self, forward: bool) {
        let query = normalize_text(&self.library_state.query);
        if query.is_empty() {
            self.info("Use / to search");
            return;
        }
        if self.view != View::Library {
            self.view = View::Library;
            self.focus = Focus::Content;
        }
        let length = self.library_state.ids.len();
        if length == 0 {
            self.info(format!("No matches for “{}”", self.library_state.query));
            return;
        }
        let library = match self.library.read() {
            Ok(library) => library,
            Err(_) => return,
        };
        let mut index = self.library_state.selected;
        let mut found = None;
        for _ in 0..length {
            index = if forward {
                (index + 1) % length
            } else {
                (index + length - 1) % length
            };
            let Some(track) = self
                .library_state
                .ids
                .get(index)
                .and_then(|id| library.find(id))
            else {
                continue;
            };
            let haystack = normalize_text(&format!(
                "{} {} {} {}",
                track.title,
                track.artist,
                track.album.as_deref().unwrap_or(""),
                track.display_genre()
            ));
            if haystack.contains(&query) {
                found = Some(index);
                break;
            }
        }
        drop(library);
        match found {
            Some(index) => self.library_state.selected = index,
            None => self.info(format!(
                "No more matches for “{}”",
                self.library_state.query
            )),
        }
    }

    // -----------------------------------------------------------------------
    // Persistence and theme
    // -----------------------------------------------------------------------

    fn save_library_now(&self) -> Result<(), String> {
        let library = self
            .library
            .read()
            .map_err(|_| "Library lock is poisoned".to_string())?;
        store::save_library(&library, &self.paths.library)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    fn save_all(&mut self) {
        let mut errors: Vec<String> = Vec::new();
        if let Err(error) = self.save_library_now() {
            errors.push(error);
        }
        if let Err(error) = self.playlists.save() {
            errors.push(error.to_string());
        }
        if let Err(error) = self.folders.save() {
            errors.push(error.to_string());
        }
        if errors.is_empty() {
            self.success("Saved library, playlists and folders");
        } else {
            self.warn(format!("Save failed: {}", errors.join("; ")));
        }
    }

    fn reload_all(&mut self) {
        let (library, report) = store::load_library(&self.paths.library);
        if let Some(error) = report.error {
            self.warn(format!("Reload failed: {error}"));
            return;
        }
        if let Ok(mut shared) = self.library.write() {
            *shared = library;
            shared.bump();
        }
        self.folders = FolderStore::load(self.paths.folders.clone());
        self.playlists = PlaylistStore::load(self.paths.playlists.clone());
        self.library_state.invalidate();
        self.recommendations.invalidate();
        self.context.revision = 0;
        self.playlists_state.loaded_name = None;
        self.sync_watch();
        self.success(format!(
            "Reloaded {} tracks · {} links",
            report.loaded, report.edges
        ));
        self.refresh();
    }

    fn prune_playlists(&mut self) {
        let removed = {
            let library = match self.library.read() {
                Ok(library) => library,
                Err(_) => return,
            };
            self.playlists.prune_missing(&library)
        };
        if removed > 0 {
            self.playlists_state.loaded_name = None;
        }
    }

    fn set_theme(&mut self, mode: ThemeMode) {
        self.theme_mode = mode;
        self.theme = Theme::for_mode(mode);
        let resolved = self.theme.resolved;
        self.success(format!("Theme: {} ({resolved})", mode.label()));
    }

    fn cycle_theme(&mut self) {
        self.set_theme(self.theme_mode.cycle());
    }

    // -----------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------

    fn execute_command(&mut self, input: &str) {
        let input = input.trim();
        if input.is_empty() {
            return;
        }
        if input.chars().all(|character| character.is_ascii_digit()) {
            self.goto_track(input);
            return;
        }
        let (command, rest) = match input.split_once(char::is_whitespace) {
            Some((command, rest)) => (command, rest.trim()),
            None => (input, ""),
        };
        match command {
            "q" | "quit" | "qa" | "q!" => self.should_quit = true,
            "w" | "write" | "save" => self.save_all(),
            "wq" | "x" => {
                self.save_all();
                self.should_quit = true;
            }
            "scan" | "rescan" | "refresh" => self.start_scan(false),
            "analyze" | "analyze!" => {
                let force = command.ends_with('!') || matches!(rest, "!" | "all" | "force");
                if force {
                    self.open_confirm(
                        "Re-analyze every track? This can take a long time.",
                        ConfirmAction::ForceAnalyze,
                    );
                } else {
                    self.start_scan(false);
                }
            }
            "theme" => {
                if rest.is_empty() {
                    self.cycle_theme();
                } else if let Some(mode) = ThemeMode::parse(rest) {
                    self.set_theme(mode);
                } else {
                    self.warn("Theme must be auto, dark or light");
                }
            }
            "watch" => match rest {
                "" | "toggle" => self.toggle_watch(),
                "on" | "enable" | "1" => {
                    self.folders.set_watch_enabled(true);
                    self.sync_watch();
                    self.success("Folder watcher enabled");
                }
                "off" | "disable" | "0" => {
                    self.folders.set_watch_enabled(false);
                    self.sync_watch();
                    self.success("Folder watcher disabled");
                }
                _ => self.warn("Usage: :watch on|off|toggle"),
            },
            "sort" => {
                if rest.is_empty() {
                    self.info(format!("Sort: {}", self.library_state.sort.label()));
                } else if let Some(sort) = SortMode::parse(rest) {
                    self.library_state.sort = sort;
                    self.library_state.invalidate();
                    if self.view != View::Library {
                        self.view = View::Library;
                    }
                    self.focus = Focus::Content;
                    self.refresh();
                    self.success(format!("Sorted by {}", sort.label()));
                } else {
                    self.warn("Sort by id, title, artist, album, year, bpm, rating or duration");
                }
            }
            "filter" | "find" => {
                self.library_state.query = rest.to_string();
                self.library_state.invalidate();
                self.view = View::Library;
                self.focus = Focus::Content;
                self.refresh();
                if rest.is_empty() {
                    self.info("Filter cleared");
                } else {
                    self.info(format!("Filter: {rest}"));
                }
            }
            "clear" => {
                self.library_state.query.clear();
                self.library_state.invalidate();
                self.info("Search cleared");
            }
            "recs" | "recommendations" | "similar" => self.action_recommendations(),
            "radio" => {
                if rest.is_empty() {
                    self.action_radio();
                } else if let Ok(limit) = rest.parse::<usize>() {
                    self.radio_with_limit(limit);
                } else {
                    self.warn("Usage: :radio [number of tracks]");
                }
            }
            "playlist" | "pl" => self.playlist_command(rest),
            "play" => {
                if rest.is_empty() {
                    self.activate_selected();
                } else if rest.chars().all(|character| character.is_ascii_digit()) {
                    self.goto_track(rest);
                    self.activate_selected();
                } else {
                    self.warn("Usage: :play [track id]");
                }
            }
            "enqueue" | "queue-add" => self.enqueue_selected(),
            "add-folder" | "folder" => {
                if rest.is_empty() {
                    self.start_prompt(PromptKind::AddFolder);
                } else {
                    match self.folders.add(rest) {
                        Ok(folder) => {
                            self.success(format!("Added folder {}", display_folder(&folder)));
                            self.sync_watch();
                            self.start_scan(false);
                        }
                        Err(error) => self.warn(error),
                    }
                }
            }
            "folders" => self.open_folders(),
            "help" => self.open_help(),
            "info" => self.open_info(),
            "status" | "analysis" => self.open_analysis(),
            "library" | "tracks" => self.switch_view_from_shortcut(View::Library),
            "for-you" | "foryou" => self.switch_view_from_shortcut(View::ForYou),
            "queue" => self.switch_view_from_shortcut(View::Queue),
            "playlists" => self.switch_view_from_shortcut(View::Playlists),
            "reload" => self.reload_all(),
            "version" => self.success(format!("{APP_NAME} {APP_VERSION}")),
            "backend" => self.info(format!("Audio backend: {}", self.player.backend_name())),
            "stop" => {
                self.player.stop();
                self.info("Playback stopped");
            }
            "next" => self.next_track(),
            "prev" | "previous" => self.previous_track(),
            "shuffle" => match rest {
                "" | "toggle" => self.toggle_shuffle(),
                "on" => {
                    if !self.player.shuffle {
                        self.player.toggle_shuffle();
                    }
                    self.success("Shuffle on");
                }
                "off" => {
                    if self.player.shuffle {
                        self.player.toggle_shuffle();
                    }
                    self.success("Shuffle off");
                }
                _ => self.warn("Usage: :shuffle on|off|toggle"),
            },
            "repeat" => {
                let value = rest.to_lowercase();
                if value.is_empty() || value == "cycle" {
                    self.cycle_repeat();
                } else if matches!(value.as_str(), "off" | "all" | "one") {
                    self.player.repeat = value;
                    self.success(format!("Repeat {}", self.player.repeat));
                } else {
                    self.warn("Usage: :repeat off|all|one");
                }
            }
            "volume" => {
                if rest.is_empty() {
                    self.info(format!("Volume {}%", self.player.volume));
                } else if let Some(volume) = parse_volume(rest, self.player.volume) {
                    self.player.set_volume(volume);
                    self.info(format!("Volume {}%", self.player.volume));
                } else {
                    self.warn("Usage: :volume 0-100 | +5 | -5");
                }
            }
            "seek" => {
                if rest.is_empty() {
                    self.info(self.player.status().position_label());
                } else if let Some(position) = parse_seek(rest, self.player.position()) {
                    match self.player.seek_to(position) {
                        Ok(()) => self.info(format!(
                            "{} / {}",
                            format_time(self.player.position()),
                            format_time(self.player.duration())
                        )),
                        Err(error) => self.player_error(error),
                    }
                } else {
                    self.warn("Usage: :seek 1:23 | +10 | -5");
                }
            }
            "clear-queue" | "queue-clear" => self.clear_queue_prompt(),
            "goto" => self.goto_track(rest),
            "remove" | "delete" => self.remove_track_prompt(),
            "redraw" => self.info("Redrawn"),
            _ => self.warn(format!("Unknown command: {command}")),
        }
    }

    fn radio_with_limit(&mut self, limit: usize) {
        let Some(seed) = self
            .context_target()
            .or_else(|| self.current_track_id())
            .or_else(|| self.library_state.selected_id().map(ToString::to_string))
        else {
            self.warn("Select a track first");
            return;
        };
        let ids = {
            let library = match self.library.read() {
                Ok(library) => library,
                Err(_) => return,
            };
            radio_track_ids(&library, &seed, limit.clamp(1, 200), &mut self.rng)
        };
        if ids.is_empty() {
            self.warn("Could not start radio");
            return;
        }
        if let Err(error) = self.player.set_queue(&ids, true) {
            self.player_error(error);
            return;
        }
        self.view = View::Queue;
        self.focus = Focus::Content;
        self.queue_selected = 0;
        self.success(format!("Radio · {} tracks", ids.len()));
    }

    fn playlist_command(&mut self, rest: &str) {
        if rest.is_empty() {
            self.switch_view_from_shortcut(View::Playlists);
            return;
        }
        let (subcommand, argument) = match rest.split_once(char::is_whitespace) {
            Some((subcommand, argument)) => (subcommand, argument.trim()),
            None => (rest, ""),
        };
        match subcommand {
            "create" | "new" => {
                if argument.is_empty() {
                    self.start_prompt(PromptKind::NewPlaylist {
                        add_ids: Vec::new(),
                    });
                } else {
                    self.create_playlist_named(argument, Vec::new());
                }
            }
            "add" => {
                if argument.is_empty() {
                    self.warn("Usage: :playlist add <name>");
                    return;
                }
                let Some(name) = self.find_playlist(argument) else {
                    self.warn(format!("Playlist “{argument}” not found"));
                    return;
                };
                if let Some(id) = self.selected_any_id() {
                    match self.playlists.add(&name, &[id]) {
                        Ok(_) => self.success(format!("Added track to “{name}”")),
                        Err(error) => self.warn(error),
                    }
                } else {
                    self.warn("Select a track first");
                }
            }
            "play" => {
                let Some(name) = self.find_playlist(argument) else {
                    self.warn(format!("Playlist “{argument}” not found"));
                    return;
                };
                if let Some(index) = self.playlists.names().iter().position(|item| item == &name) {
                    self.playlists_state.select_playlist(index);
                }
                self.view = View::Playlists;
                self.focus = Focus::Content;
                self.playlist_pane = PlaylistPane::Tracks;
                self.refresh();
                self.play_playlist_from(0);
            }
            "delete" | "remove" => {
                if argument.is_empty() {
                    self.delete_playlist_prompt();
                    return;
                }
                let Some(name) = self.find_playlist(argument) else {
                    self.warn(format!("Playlist “{argument}” not found"));
                    return;
                };
                self.open_confirm(
                    format!("Delete playlist “{name}”?"),
                    ConfirmAction::DeletePlaylist(name),
                );
            }
            "rename" => {
                if argument.is_empty() {
                    self.rename_playlist_prompt();
                    return;
                }
                let Some(old) = self.current_playlist_name() else {
                    self.warn("No playlist selected");
                    return;
                };
                match self.playlists.rename(&old, argument) {
                    Ok(name) => {
                        if let Some(index) =
                            self.playlists.names().iter().position(|item| item == &name)
                        {
                            self.playlists_state.select_playlist(index);
                        }
                        self.playlists_state.loaded_name = None;
                        self.success(format!("Renamed playlist to “{name}”"));
                        self.refresh();
                    }
                    Err(error) => self.warn(error),
                }
            }
            "clear" => {
                let Some(name) = self.find_playlist(argument) else {
                    self.warn(format!("Playlist “{argument}” not found"));
                    return;
                };
                if self.playlists.clear(&name) {
                    self.playlists_state.loaded_name = None;
                    self.success(format!("Cleared “{name}”"));
                    self.refresh();
                }
            }
            _ => {
                let Some(name) = self.find_playlist(rest) else {
                    self.warn(format!("Playlist “{rest}” not found"));
                    return;
                };
                if let Some(index) = self.playlists.names().iter().position(|item| item == &name) {
                    self.playlists_state.select_playlist(index);
                }
                self.view = View::Playlists;
                self.focus = Focus::Content;
                self.playlist_pane = PlaylistPane::Tracks;
                self.refresh();
            }
        }
    }

    fn create_playlist_named(&mut self, name: &str, add_ids: Vec<String>) {
        match self.playlists.create(name) {
            Ok(name) => {
                if !add_ids.is_empty() {
                    let _ = self.playlists.add(&name, &add_ids);
                }
                if let Some(index) = self.playlists.names().iter().position(|item| item == &name) {
                    self.playlists_state.select_playlist(index);
                }
                self.view = View::Playlists;
                self.focus = Focus::Content;
                self.playlist_pane = PlaylistPane::List;
                self.success(format!("Playlist “{name}” created"));
                self.refresh();
            }
            Err(error) => self.warn(error),
        }
    }

    fn find_playlist(&self, name: &str) -> Option<String> {
        let names = self.playlists.names();
        names
            .iter()
            .find(|item| item.as_str() == name)
            .cloned()
            .or_else(|| {
                let lowered = name.to_lowercase();
                names
                    .iter()
                    .find(|item| item.to_lowercase() == lowered)
                    .cloned()
            })
    }

    fn goto_track(&mut self, value: &str) {
        let value = value.trim();
        if value.is_empty() {
            self.warn("Usage: :goto <track id>");
            return;
        }
        self.library_state.query.clear();
        self.library_state.invalidate();
        self.view = View::Library;
        self.focus = Focus::Content;
        self.refresh();
        if let Some(index) = self.library_state.ids.iter().position(|id| id == value) {
            self.library_state.selected = index;
            self.success(format!("Selected {}", self.label_for(value)));
        } else {
            self.warn(format!("Track id {value} not found"));
        }
    }

    // -----------------------------------------------------------------------
    // Background worker and playback updates
    // -----------------------------------------------------------------------

    pub fn tick(&mut self) {
        self.player.tick();
        let player_status = self.player.status();
        if player_status.error.is_some() && player_status.error != self.last_player_error {
            self.last_player_error = player_status.error.clone();
            if let Some(error) = player_status.error {
                self.warn(error);
            }
        }
        for message in self.worker.drain() {
            self.handle_worker_message(message);
        }
        self.refresh();
    }

    fn handle_worker_message(&mut self, message: WorkerMessage) {
        match message {
            WorkerMessage::Started { auto, message } => {
                self.status_kind = StatusKind::Working;
                self.status = if auto {
                    format!("Folder watcher: {message}")
                } else {
                    message
                };
            }
            WorkerMessage::WatchTriggered(message) => {
                self.info(message);
            }
            WorkerMessage::Progress(progress) => {
                self.scan = Some(progress);
                self.status_kind = StatusKind::Working;
            }
            WorkerMessage::Finished(outcome) => self.finish_scan(outcome),
            WorkerMessage::Error(error) => {
                self.scan = None;
                self.warn(error);
                self.run_pending_scan();
            }
        }
    }

    fn finish_scan(&mut self, outcome: ScanOutcome) {
        self.scan = None;
        for id in &outcome.removed_ids {
            self.player.remove_track(id);
        }
        self.prune_playlists();
        self.library_state.invalidate();
        self.recommendations.invalidate();
        self.context.revision = 0;
        self.playlists_state.loaded_name = None;
        if let Some(error) = outcome.error {
            self.warn(format!("Scan finished with an error: {error}"));
            self.run_pending_scan();
            return;
        }
        let mut text = format!(
            "Scan complete: {} tracks · {} links",
            outcome.tracks, outcome.edges
        );
        if outcome.loaded > 0 || outcome.updated > 0 {
            text.push_str(&format!(
                " · {} new, {} updated",
                outcome.loaded, outcome.updated
            ));
        }
        if outcome.analyzed > 0 {
            text.push_str(&format!(" · {} analyzed", outcome.analyzed));
        }
        if outcome.failed > 0 {
            text.push_str(&format!(" · {} failed", outcome.failed));
        }
        if !outcome.removed_ids.is_empty() {
            text.push_str(&format!(" · {} removed", outcome.removed_ids.len()));
        }
        self.success(text);
        self.refresh();
        self.run_pending_scan();
    }

    fn run_pending_scan(&mut self) {
        if let Some(force) = self.pending_scan.take() {
            self.start_scan(force);
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn tail_log(path: &Path, count: usize) -> Vec<String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .take(count)
        .map(ToString::to_string)
        .collect();
    lines.reverse();
    lines
}

pub fn parse_volume(value: &str, current: i32) -> Option<i32> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('+') {
        return rest
            .parse::<i32>()
            .ok()
            .map(|delta| (current + delta).clamp(0, 100));
    }
    if let Some(rest) = value.strip_prefix('-') {
        return rest
            .parse::<i32>()
            .ok()
            .map(|delta| (current - delta).clamp(0, 100));
    }
    value
        .trim_end_matches('%')
        .parse::<i32>()
        .ok()
        .map(|volume| volume.clamp(0, 100))
}

pub fn parse_seek(value: &str, current: f64) -> Option<f64> {
    let value = value.trim();
    if let Some(rest) = value.strip_prefix('+') {
        return parse_time(rest).map(|seconds| current + seconds);
    }
    if let Some(rest) = value.strip_prefix('-') {
        return parse_time(rest).map(|seconds| (current - seconds).max(0.0));
    }
    parse_time(value)
}

fn parse_time(value: &str) -> Option<f64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if let Some((minutes, seconds)) = value.split_once(':') {
        let minutes: f64 = minutes.parse().ok()?;
        let seconds: f64 = seconds.parse().ok()?;
        Some((minutes * 60.0 + seconds).max(0.0))
    } else {
        value.parse::<f64>().ok().map(|seconds| seconds.max(0.0))
    }
}

pub fn folder_track_count(library: &Library, folder: &str) -> usize {
    library
        .tracks
        .iter()
        .filter(|track| {
            track
                .filepath
                .as_deref()
                .map(|path| path_is_within(path, folder))
                .unwrap_or(false)
        })
        .count()
}

pub fn file_size_label(track: &Track) -> String {
    match track.file_size {
        Some(size) if size >= 1_048_576 => format!("{:.1} MB", size as f64 / 1_048_576.0),
        Some(size) if size >= 1024 => format!("{:.0} KB", size as f64 / 1024.0),
        Some(size) => format!("{size} B"),
        None => "—".to_string(),
    }
}

pub fn track_path_label(track: &Track) -> String {
    track
        .filepath
        .as_deref()
        .map(|path| {
            let path = PathBuf::from(path);
            path.to_string_lossy().into_owned()
        })
        .unwrap_or_else(|| "—".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_viewport_moves_the_view_without_touching_a_visible_selection() {
        let (mut selected, mut offset) = (5, 0);
        scroll_viewport(&mut selected, &mut offset, 100, 10, WHEEL_STEP);
        assert_eq!((selected, offset), (5, 3));
        scroll_viewport(&mut selected, &mut offset, 100, 10, -WHEEL_STEP);
        assert_eq!((selected, offset), (5, 0));
    }

    #[test]
    fn scroll_viewport_pulls_the_selection_when_it_leaves_the_view() {
        // Wheel down with the selection at the top edge pulls it to the new
        // first visible row.
        let (mut selected, mut offset) = (5, 5);
        scroll_viewport(&mut selected, &mut offset, 100, 10, WHEEL_STEP);
        assert_eq!((selected, offset), (8, 8));

        // Wheel up with the selection at the bottom edge pulls it to the new
        // last visible row.
        let (mut selected, mut offset) = (14, 5);
        scroll_viewport(&mut selected, &mut offset, 100, 10, -WHEEL_STEP);
        assert_eq!((selected, offset), (11, 2));

        // Scrolling past the end clamps the viewport and keeps the selection.
        let (mut selected, mut offset) = (95, 88);
        scroll_viewport(&mut selected, &mut offset, 100, 10, WHEEL_STEP);
        assert_eq!((selected, offset), (95, 90));
        scroll_viewport(&mut selected, &mut offset, 100, 10, WHEEL_STEP);
        assert_eq!((selected, offset), (95, 90));
    }

    #[test]
    fn scroll_viewport_handles_empty_and_short_lists() {
        let (mut selected, mut offset) = (5, 5);
        scroll_viewport(&mut selected, &mut offset, 0, 10, WHEEL_STEP);
        assert_eq!((selected, offset), (0, 0));

        let (mut selected, mut offset) = (2, 0);
        scroll_viewport(&mut selected, &mut offset, 3, 10, -WHEEL_STEP);
        assert_eq!((selected, offset), (2, 0));
    }

    #[test]
    fn ensure_visible_scrolls_only_when_the_selection_leaves() {
        let mut offset = 5;
        ensure_visible(6, &mut offset, 100, 10);
        assert_eq!(offset, 5);
        ensure_visible(15, &mut offset, 100, 10);
        assert_eq!(offset, 6);
        ensure_visible(14, &mut offset, 100, 10);
        assert_eq!(offset, 6);
        ensure_visible(50, &mut offset, 100, 10);
        assert_eq!(offset, 41);
        ensure_visible(99, &mut offset, 100, 10);
        assert_eq!(offset, 90);
        ensure_visible(0, &mut offset, 100, 10);
        assert_eq!(offset, 0);
    }

    fn hitbox() -> TableHitbox {
        TableHitbox {
            view: View::Library,
            pane: PlaylistPane::Tracks,
            x: 10,
            y: 5,
            width: 20,
            height: 8,
            offset: 100,
            rows: 200,
        }
    }

    #[test]
    fn table_hitbox_maps_rows_to_track_indices() {
        let hitbox = hitbox();
        assert!(hitbox.contains(10, 5));
        assert!(hitbox.contains(29, 12));
        assert!(!hitbox.contains(9, 5));
        assert!(!hitbox.contains(10, 4));
        assert!(!hitbox.contains(10, 13));
        assert_eq!(hitbox.track_index(5), Some(100));
        assert_eq!(hitbox.track_index(12), Some(107));
        assert_eq!(hitbox.track_index(4), None);
        assert_eq!(hitbox.track_index(13), None);
    }

    #[test]
    fn table_hitbox_ignores_rows_past_the_last_track() {
        let mut hitbox = hitbox();
        hitbox.rows = 104;
        assert_eq!(hitbox.track_index(8), Some(103));
        assert_eq!(hitbox.track_index(9), None);
    }
}
