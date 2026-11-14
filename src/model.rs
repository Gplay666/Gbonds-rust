use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::graph::Graph;
use crate::paths::id_sort_key;

pub type Analysis = Map<String, Value>;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Track {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default)]
    pub metadata_genre: String,
    #[serde(default)]
    pub duration: i64,
    #[serde(default = "default_rating")]
    pub rating: i32,
    #[serde(default)]
    pub filepath: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default)]
    pub year: Option<i32>,
    #[serde(default)]
    pub track_number: Option<i32>,
    #[serde(default)]
    pub file_mtime_ns: Option<i64>,
    #[serde(default)]
    pub file_size: Option<u64>,
    #[serde(default)]
    pub analysis: Option<Analysis>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_rating() -> i32 {
    5
}

impl Track {
    pub fn has_analysis(&self) -> bool {
        self.analysis.is_some()
    }

    pub fn playable(&self) -> bool {
        self.filepath
            .as_deref()
            .map(|path| !path.is_empty())
            .unwrap_or(false)
    }

    pub fn analysis_f64(&self, field: &str) -> f64 {
        self.analysis
            .as_ref()
            .and_then(|analysis| analysis.get(field))
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .unwrap_or(0.0)
    }

    pub fn analysis_str<'a>(&'a self, field: &str) -> Option<&'a str> {
        self.analysis
            .as_ref()
            .and_then(|analysis| analysis.get(field))
            .and_then(Value::as_str)
    }

    pub fn bpm(&self) -> f64 {
        self.analysis_f64("bpm")
    }

    pub fn mood(&self) -> &str {
        self.analysis_str("mood").unwrap_or("")
    }

    pub fn key(&self) -> &str {
        self.analysis_str("key").unwrap_or("")
    }

    pub fn scale(&self) -> &str {
        self.analysis_str("scale").unwrap_or("")
    }

    pub fn audio_genre(&self) -> &str {
        self.analysis_str("audio_genre").unwrap_or("")
    }

    pub fn audio_genre_family(&self) -> &str {
        self.analysis_str("audio_genre_family").unwrap_or("")
    }

    pub fn duration_seconds(&self) -> f64 {
        if self.duration > 0 {
            self.duration as f64
        } else {
            self.analysis_f64("duration_audio")
        }
    }

    /// Genre family used by the metadata similarity function, mirroring
    /// `apply_analysis` in the Python player.
    pub fn genre_family(&self) -> String {
        let raw = if self.metadata_genre.trim().is_empty() {
            self.genre.as_str()
        } else {
            self.metadata_genre.as_str()
        };
        let (display, family) = normalize_genre(raw);
        if !display.is_empty() {
            return family;
        }
        let audio = self.audio_genre_family();
        if audio.is_empty() {
            "Unknown".to_string()
        } else {
            audio.to_string()
        }
    }

    pub fn display_genre(&self) -> &str {
        if !self.genre.is_empty() {
            &self.genre
        } else {
            self.audio_genre()
        }
    }

    pub fn signature(&self) -> Option<(i64, u64)> {
        Some((self.file_mtime_ns?, self.file_size?))
    }

    pub fn sort_key(&self) -> (u8, i64, String) {
        id_sort_key(&self.id)
    }

    pub fn title_artist(&self) -> String {
        if self.artist.trim().is_empty() {
            self.title.clone()
        } else {
            format!("{} — {}", self.artist, self.title)
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Library {
    pub tracks: Vec<Track>,
    pub graph: Graph,
    index: HashMap<String, usize>,
    pub revision: u64,
    pub saved_at: Option<String>,
    pub extra: Map<String, Value>,
}

impl Library {
    pub fn new() -> Self {
        let mut library = Self::default();
        library.revision = 1;
        library
    }

    pub fn rebuild_index(&mut self) {
        self.index.clear();
        for (position, track) in self.tracks.iter().enumerate() {
            self.index.insert(track.id.clone(), position);
        }
        self.bump();
    }

    pub fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn find(&self, id: &str) -> Option<&Track> {
        self.index
            .get(id)
            .and_then(|position| self.tracks.get(*position))
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut Track> {
        let position = *self.index.get(id)?;
        self.tracks.get_mut(position)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.index.contains_key(id)
    }

    pub fn add_track(&mut self, track: Track) -> bool {
        if self.contains(&track.id) {
            return false;
        }
        self.graph.add_node(track.id.clone());
        self.index.insert(track.id.clone(), self.tracks.len());
        self.tracks.push(track);
        self.bump();
        true
    }

    pub fn remove_track(&mut self, id: &str) -> Option<Track> {
        let position = self.index.remove(id)?;
        let track = self.tracks.remove(position);
        self.graph.remove_node(id);
        self.rebuild_index();
        Some(track)
    }

    pub fn playable_ids_sorted(&self) -> Vec<String> {
        let mut tracks: Vec<&Track> = self
            .tracks
            .iter()
            .filter(|track| track.playable())
            .collect();
        tracks.sort_by_key(|track| track.sort_key());
        tracks.into_iter().map(|track| track.id.clone()).collect()
    }

    pub fn max_numeric_id(&self) -> i64 {
        self.tracks
            .iter()
            .filter_map(|track| track.id.parse::<i64>().ok())
            .max()
            .unwrap_or(0)
    }

    pub fn next_free_id(&self, start: i64) -> i64 {
        let mut candidate = start;
        while self.contains(&candidate.to_string()) {
            candidate += 1;
        }
        candidate
    }

    pub fn existing_fingerprints(
        &self,
    ) -> std::collections::HashSet<(String, String, String, i64)> {
        self.tracks
            .iter()
            .filter(|track| {
                track
                    .album
                    .as_deref()
                    .map(|album| !album.is_empty() && album != "Unknown")
                    .unwrap_or(false)
            })
            .map(fingerprint)
            .collect()
    }
}

pub fn canonicalish_string(path: &str) -> String {
    crate::paths::canonicalish(std::path::Path::new(path))
        .to_string_lossy()
        .into_owned()
}

pub fn fingerprint(track: &Track) -> (String, String, String, i64) {
    (
        track.artist.trim().to_lowercase(),
        track.title.trim().to_lowercase(),
        track.album.as_deref().unwrap_or("").trim().to_lowercase(),
        track.duration,
    )
}

pub fn normalize_text(value: &str) -> String {
    let lowered = value.trim().to_lowercase();
    let mut out = String::with_capacity(lowered.len());
    let mut last_space = true;
    for ch in lowered.chars() {
        if ch.is_alphanumeric() {
            out.push(ch);
            last_space = false;
        } else if !last_space {
            out.push(' ');
            last_space = true;
        }
    }
    out.trim().to_string()
}

/// Returns `(canonical display genre, broad family)`.
pub fn normalize_genre(raw_genre: &str) -> (String, String) {
    let raw = raw_genre.replace('\u{feff}', "");
    let raw = raw.trim();
    if raw.is_empty() {
        return (String::new(), String::new());
    }

    let mut key = raw.to_lowercase();
    key = key.split_whitespace().collect::<Vec<_>>().join(" ");
    key = key.replace('_', " ");

    let alias = match key.as_str() {
        "punk" => Some(("Punk", "Rock")),
        "punk rock" | "punkrock" => Some(("Punk Rock", "Rock")),
        "hardcore" => Some(("Hardcore", "Rock")),
        "allrock" => Some(("Rock", "Rock")),
        "rusrock" | "russian rock" => Some(("Russian Rock", "Rock")),
        "rock" => Some(("Rock", "Rock")),
        "grunge" => Some(("Grunge", "Rock")),
        "ska" => Some(("Ska", "Ska")),
        "folkrock" | "folk rock" => Some(("Folk Rock", "Folk")),
        "folk" => Some(("Folk", "Folk")),
        "metal" => Some(("Metal", "Metal")),
        "heavy metal" => Some(("Heavy Metal", "Metal")),
        "nu metal" => Some(("Nu Metal", "Metal")),
        "thrash metal" => Some(("Thrash Metal", "Metal")),
        "alternative" => Some(("Alternative", "Alternative")),
        "local-indie" | "indie" => Some(("Indie", "Alternative")),
        "pop" => Some(("Pop", "Pop")),
        "dance" => Some(("Dance", "Electronic")),
        "electronic" | "edm" => Some(("Electronic", "Electronic")),
        "hip-hop" | "hip hop" => Some(("Hip-Hop", "Hip-Hop")),
        "rap" => Some(("Rap", "Hip-Hop")),
        "jazz" => Some(("Jazz", "Jazz")),
        "blues" => Some(("Blues", "Jazz")),
        "classical" => Some(("Classical", "Classical")),
        "ambient" => Some(("Ambient", "Ambient")),
        "soundtrack" => Some(("Soundtrack", "Soundtrack")),
        _ => None,
    };
    if let Some((display, family)) = alias {
        return (display.to_string(), family.to_string());
    }

    if key.contains("punk") {
        return ("Punk".into(), "Rock".into());
    }
    if key.contains("metal") {
        return ("Metal".into(), "Metal".into());
    }
    if key.contains("rock") {
        return ("Rock".into(), "Rock".into());
    }
    if key.contains("indie") {
        return ("Indie".into(), "Alternative".into());
    }
    if key.contains("ska") {
        return ("Ska".into(), "Ska".into());
    }
    if key.contains("folk") {
        return ("Folk".into(), "Folk".into());
    }
    if key.contains("pop") {
        return ("Pop".into(), "Pop".into());
    }
    if key.contains("electro") || key.contains("dance") || key.contains("edm") {
        return ("Electronic".into(), "Electronic".into());
    }
    if key.contains("hip") || key.contains("rap") {
        return ("Hip-Hop".into(), "Hip-Hop".into());
    }
    if key.contains("jazz") || key.contains("blues") || key.contains("swing") {
        return ("Jazz/Blues".into(), "Jazz".into());
    }
    if key.contains("classic") || key.contains("orchestra") || key.contains("opera") {
        return ("Classical".into(), "Classical".into());
    }
    if key.contains("ambient") || key.contains("chill") {
        return ("Ambient".into(), "Ambient".into());
    }

    let mut chars = raw.chars();
    let display = match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    };
    (display.clone(), display)
}
