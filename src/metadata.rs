use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

pub const AUDIO_EXTENSIONS: &[&str] = &[
    ".mp3", ".flac", ".ogg", ".oga", ".opus", ".wav", ".m4a", ".aac", ".wma", ".aiff", ".aif",
    ".ape", ".alac",
];

#[derive(Clone, Debug)]
pub struct AudioMeta {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub year: Option<i32>,
    pub track_number: Option<i32>,
    pub duration: i64,
}

pub fn probe_file(path: &Path) -> Option<Value> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    serde_json::from_slice(&output.stdout).ok()
}

pub fn has_audio_stream(probe: &Value) -> bool {
    probe
        .get("streams")
        .and_then(Value::as_array)
        .map(|streams| {
            streams
                .iter()
                .any(|stream| stream.get("codec_type").and_then(Value::as_str) == Some("audio"))
        })
        .unwrap_or(false)
}

fn stringify(value: &Value) -> String {
    match value {
        Value::String(text) => text.replace('\u{feff}', "").trim().to_string(),
        Value::Null => String::new(),
        other => other.to_string().trim_matches('"').trim().to_string(),
    }
}

fn tags(probe: &Value) -> HashMap<String, String> {
    let mut result = HashMap::new();
    let Some(raw) = probe
        .get("format")
        .and_then(|format| format.get("tags"))
        .and_then(Value::as_object)
    else {
        return result;
    };
    for (key, value) in raw {
        let value = stringify(value);
        if !value.is_empty() {
            result.insert(key.to_lowercase(), value);
        }
    }
    result
}

fn tag<'a>(tags: &'a HashMap<String, String>, names: &[&str]) -> &'a str {
    for name in names {
        if let Some(value) = tags.get(&name.to_lowercase()) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    ""
}

fn get_extension(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let Some(position) = name.rfind('.') else {
        return String::new();
    };
    let extension = name[position..].to_lowercase();
    if extension.chars().any(char::is_whitespace) {
        String::new()
    } else {
        extension
    }
}

pub fn is_audio_candidate(path: &Path) -> bool {
    let extension = get_extension(path);
    AUDIO_EXTENSIONS.contains(&extension.as_str()) || extension.is_empty()
}

pub fn has_audio_extension(path: &Path) -> bool {
    AUDIO_EXTENSIONS.contains(&get_extension(path).as_str())
}

fn strip_track_prefix(stem: &str) -> &str {
    let trimmed = stem.trim_start();
    let digits: String = trimmed.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return trimmed;
    }
    let rest = &trimmed[digits.len()..];
    let rest = rest.strip_prefix('.').or_else(|| rest.strip_prefix(')'));
    match rest {
        Some(rest) if !rest.is_empty() => rest.trim_start(),
        _ => trimmed,
    }
}

fn filename_parts(filename: &str) -> (String, String) {
    let path = Path::new(filename);
    let extension = get_extension(path);
    let stem = if extension.is_empty() {
        filename
    } else {
        filename
            .strip_suffix(&extension)
            .unwrap_or(filename)
            .trim_end_matches('.')
    };
    let stem = strip_track_prefix(stem);
    if let Some((artist, title)) = stem.split_once(" - ") {
        return (artist.trim().to_string(), title.trim().to_string());
    }
    (String::new(), stem.trim().to_string())
}

fn has_cyrillic(value: &str) -> bool {
    value
        .chars()
        .any(|ch| ('\u{0400}'..='\u{04ff}').contains(&ch))
}

fn prefer_readable(tag_value: &str, filename_value: &str) -> String {
    if tag_value.is_empty() {
        return filename_value.to_string();
    }
    if filename_value.is_empty() {
        return tag_value.to_string();
    }
    if has_cyrillic(filename_value) && !has_cyrillic(tag_value) {
        return filename_value.to_string();
    }
    tag_value.to_string()
}

fn parse_year(value: &str) -> Option<i32> {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() < 4 {
        return None;
    }
    for window in chars.windows(4) {
        if window.iter().all(char::is_ascii_digit) {
            return window.iter().collect::<String>().parse().ok();
        }
    }
    None
}

fn parse_track_number(value: &str) -> Option<i32> {
    let mut digits = String::new();
    let mut started = false;
    for ch in value.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            started = true;
        } else if started {
            break;
        }
    }
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

pub fn extract_metadata(probe: &Value, filename: &str) -> Option<AudioMeta> {
    if !has_audio_stream(probe) {
        return None;
    }
    let tags = tags(probe);
    let (file_artist, file_title) = filename_parts(filename);

    let mut title = tag(&tags, &["title"]).to_string();
    let mut artist = tag(&tags, &["artist", "album_artist", "albumartist"]).to_string();
    let album = tag(&tags, &["album"]).to_string();
    let genre = tag(&tags, &["genre"]).to_string();
    let date = tag(&tags, &["date", "year"]).to_string();
    let track_number = tag(&tags, &["track", "tracknumber"]).to_string();

    title = prefer_readable(&title, &file_title);
    artist = prefer_readable(&artist, &file_artist);
    if title.is_empty() {
        title = file_title;
    }
    if title.is_empty() {
        title = Path::new(filename)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| filename.to_string());
    }
    if artist.is_empty() {
        artist = "Unknown".to_string();
    }

    let duration = probe
        .get("format")
        .and_then(|format| format.get("duration"))
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })
        .unwrap_or(0.0)
        .round() as i64;

    Some(AudioMeta {
        title,
        artist,
        album: if album.is_empty() {
            "Unknown".to_string()
        } else {
            album
        },
        genre,
        year: parse_year(&date),
        track_number: parse_track_number(&track_number),
        duration,
    })
}

pub fn probe_metadata(path: &Path) -> Result<AudioMeta, String> {
    let probe =
        probe_file(path).ok_or_else(|| format!("Cannot probe audio file: {}", path.display()))?;
    extract_metadata(&probe, &path.to_string_lossy())
        .ok_or_else(|| format!("No audio stream in {}", path.display()))
}
