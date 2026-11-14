use std::env;
use std::fs;
use std::path::{Path, PathBuf};

/// gbonds keeps its own cache. The Python TUIMusicRecomendator cache is only
/// read once to seed this directory on first launch.
pub const APP_DIR_NAME: &str = "gbonds";
pub const LEGACY_APP_DIR_NAME: &str = "tui-music-recomendator";
const CACHE_FILES: &[&str] = &[
    "library.json",
    "folders.json",
    "playlists.json",
    "analysis_state.json",
    "analysis.log",
];

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub data_dir: PathBuf,
    pub library: PathBuf,
    pub folders: PathBuf,
    pub playlists: PathBuf,
    pub analysis_log: PathBuf,
    pub analysis_state: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Self {
        let explicit = env::var_os("GBONDS_DATA_DIR").or_else(|| env::var_os("TUI_MUSIC_DATA_DIR"));
        let data_dir = match explicit {
            Some(path) => PathBuf::from(path),
            None => {
                let base = absolute_env_path("XDG_DATA_HOME")
                    .unwrap_or_else(|| home_dir().join(".local/share"));
                let data_dir = base.join(APP_DIR_NAME);
                migrate_legacy_cache(&data_dir, &base.join(LEGACY_APP_DIR_NAME));
                data_dir
            }
        };

        Self::from_data_dir(data_dir)
    }

    pub fn from_data_dir(data_dir: PathBuf) -> Self {
        let explicit = |gbonds: &str, tui: &str, fallback: PathBuf| {
            env::var_os(gbonds)
                .or_else(|| env::var_os(tui))
                .map(PathBuf::from)
                .map(expand_user)
                .unwrap_or(fallback)
        };

        let library = explicit(
            "GBONDS_LIBRARY",
            "TUI_MUSIC_LIBRARY",
            data_dir.join("library.json"),
        );
        let folders = explicit(
            "GBONDS_FOLDERS",
            "TUI_MUSIC_FOLDERS",
            data_dir.join("folders.json"),
        );
        let playlists = explicit(
            "GBONDS_PLAYLISTS",
            "TUI_MUSIC_PLAYLISTS",
            data_dir.join("playlists.json"),
        );
        let analysis_log = explicit("GBONDS_LOG", "TUI_MUSIC_LOG", data_dir.join("analysis.log"));
        let analysis_state = explicit(
            "GBONDS_ANALYSIS_STATE",
            "TUI_MUSIC_ANALYSIS_STATE",
            data_dir.join("analysis_state.json"),
        );

        Self {
            data_dir,
            library,
            folders,
            playlists,
            analysis_log,
            analysis_state,
        }
    }
}

/// Copies the Python player's cache into gbonds' own directory once. Only
/// missing files are copied, so an existing gbonds cache is never replaced.
fn migrate_legacy_cache(data_dir: &Path, legacy_dir: &Path) {
    if data_dir == legacy_dir || !legacy_dir.join("library.json").is_file() {
        return;
    }
    if fs::create_dir_all(data_dir).is_err() {
        return;
    }
    for name in CACHE_FILES {
        let source = legacy_dir.join(name);
        let target = data_dir.join(name);
        if source.is_file() && !target.exists() {
            let _ = fs::copy(&source, &target);
        }
    }
}

pub fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn absolute_env_path(name: &str) -> Option<PathBuf> {
    let value = env::var_os(name)?;
    let path = PathBuf::from(value);
    path.is_absolute().then_some(path)
}

pub fn expand_user(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    let text = path.to_string_lossy();
    if text == "~" {
        return home_dir();
    }
    if let Some(rest) = text.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    if text.starts_with("~") {
        // Best effort for `~user/...`: leave it alone.
        return path.to_path_buf();
    }
    path.to_path_buf()
}

pub fn lexical_absolute(path: &Path) -> PathBuf {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in path.components() {
        use std::path::Component;
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new("/")),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        out
    }
}

/// Resolves symlinks when possible, falling back to a canonicalized parent
/// directory and finally to lexical normalization. This keeps deleted files
/// comparable with an existing root.
pub fn canonicalish(path: &Path) -> PathBuf {
    let expanded = expand_user(path);
    if let Ok(path) = expanded.canonicalize() {
        return path;
    }
    if let (Some(parent), Some(name)) = (expanded.parent(), expanded.file_name()) {
        if let Ok(parent) = parent.canonicalize() {
            return parent.join(name);
        }
    }
    lexical_absolute(&expanded)
}

pub fn normalize_folder(path: impl AsRef<Path>) -> String {
    canonicalish(path.as_ref()).to_string_lossy().into_owned()
}

pub fn path_is_within(path: impl AsRef<Path>, root: impl AsRef<Path>) -> bool {
    if path.as_ref().as_os_str().is_empty() || root.as_ref().as_os_str().is_empty() {
        return false;
    }
    let candidate = canonicalish(path.as_ref());
    let root = canonicalish(root.as_ref());
    candidate.starts_with(root)
}

pub fn display_folder(path: &str) -> String {
    let home = home_dir();
    let home = home.to_string_lossy();
    if path == home {
        return "~".to_string();
    }
    if let Some(rest) = path.strip_prefix(&format!("{home}/")) {
        return format!("~/{rest}");
    }
    path.to_string()
}

pub fn format_time(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as u64;
    let (hours, minutes, secs) = (total / 3600, (total % 3600) / 60, total % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// Numeric track ids sort numerically, non-numeric ids after them.
pub fn id_sort_key(id: &str) -> (u8, i64, String) {
    match id.parse::<i64>() {
        Ok(number) => (0, number, String::new()),
        Err(_) => (1, 0, id.to_string()),
    }
}

pub fn utc_now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn scratch_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = env::temp_dir().join(format!("gbonds-{name}-{}-{nanos}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn legacy_cache_is_copied_once_without_overwriting() {
        let root = scratch_dir("migrate");
        let legacy = root.join("legacy");
        let data = root.join("gbonds");
        fs::create_dir_all(&legacy).unwrap();
        fs::write(legacy.join("library.json"), "old-library").unwrap();
        fs::write(legacy.join("folders.json"), "old-folders").unwrap();
        fs::write(legacy.join("playlists.json"), "old-playlists").unwrap();
        fs::write(legacy.join("notes.txt"), "ignored").unwrap();

        migrate_legacy_cache(&data, &legacy);
        assert_eq!(
            fs::read_to_string(data.join("library.json")).unwrap(),
            "old-library"
        );
        assert_eq!(
            fs::read_to_string(data.join("folders.json")).unwrap(),
            "old-folders"
        );
        assert_eq!(
            fs::read_to_string(data.join("playlists.json")).unwrap(),
            "old-playlists"
        );
        assert!(!data.join("notes.txt").exists());

        // Existing gbonds files always win over the legacy copy.
        fs::write(data.join("folders.json"), "gbonds-folders").unwrap();
        fs::write(legacy.join("folders.json"), "changed-legacy").unwrap();
        migrate_legacy_cache(&data, &legacy);
        assert_eq!(
            fs::read_to_string(data.join("folders.json")).unwrap(),
            "gbonds-folders"
        );
        assert_eq!(
            fs::read_to_string(data.join("library.json")).unwrap(),
            "old-library"
        );

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn migration_does_nothing_without_a_legacy_library() {
        let root = scratch_dir("no-legacy");
        let data = root.join("gbonds");
        migrate_legacy_cache(&data, &root.join("legacy"));
        assert!(!data.exists());
        fs::remove_dir_all(&root).unwrap();
    }
}
