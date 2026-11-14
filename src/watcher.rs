use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::UNIX_EPOCH;

use walkdir::WalkDir;

use crate::metadata::is_audio_candidate;
use crate::paths::{canonicalish, normalize_folder, path_is_within};

pub type FileSignature = (i64, u64);

#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub roots: HashMap<String, bool>,
    pub files: HashMap<String, FileSignature>,
    pub root_errors: HashSet<String>,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Change {
    pub added: HashSet<String>,
    pub removed: HashSet<String>,
    pub modified: HashSet<String>,
}

impl Change {
    pub fn has_changes(&self) -> bool {
        !self.added.is_empty() || !self.removed.is_empty() || !self.modified.is_empty()
    }
}

fn signature(path: &Path) -> Option<FileSignature> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    let duration = modified.duration_since(UNIX_EPOCH).ok()?;
    Some((duration.as_nanos() as i64, metadata.len()))
}

pub fn collect_snapshot(folders: &[String]) -> Snapshot {
    let mut snapshot = Snapshot::default();
    for raw_root in folders {
        let root = normalize_folder(raw_root);
        if snapshot.roots.contains_key(&root) {
            continue;
        }
        let exists = Path::new(&root).is_dir();
        snapshot.roots.insert(root.clone(), exists);
        if !exists {
            continue;
        }
        let mut walk_error = false;
        for entry in WalkDir::new(&root).follow_links(false).into_iter() {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    walk_error = true;
                    snapshot.errors.push(format!("{root}: {error}"));
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            if !is_audio_candidate(entry.path()) {
                continue;
            }
            let path = canonicalish(entry.path()).to_string_lossy().into_owned();
            if let Some(signature) = signature(entry.path()) {
                snapshot.files.insert(path, signature);
            } else {
                snapshot
                    .errors
                    .push(format!("Cannot stat {}", entry.path().display()));
            }
        }
        if walk_error {
            snapshot.root_errors.insert(root);
        }
    }
    snapshot
}

pub fn library_baseline(
    tracks: impl IntoIterator<Item = (String, Option<FileSignature>)>,
    roots: &HashMap<String, bool>,
) -> HashMap<String, FileSignature> {
    let existing_roots: Vec<&String> = roots
        .iter()
        .filter(|(_, exists)| **exists)
        .map(|(root, _)| root)
        .collect();
    let mut result = HashMap::new();
    for (path, signature) in tracks {
        if let Some(signature) = signature {
            if existing_roots
                .iter()
                .any(|root| path_is_within(&path, root))
            {
                result.insert(path, signature);
            }
        }
    }
    result
}

pub fn diff(
    baseline: &HashMap<String, FileSignature>,
    current: &HashMap<String, FileSignature>,
    roots: &HashMap<String, bool>,
    root_errors: &HashSet<String>,
) -> Change {
    let existing_roots: Vec<&String> = roots
        .iter()
        .filter(|(_, exists)| **exists)
        .map(|(root, _)| root)
        .collect();

    let mut change = Change::default();
    for (path, signature) in current {
        match baseline.get(path) {
            None => {
                change.added.insert(path.clone());
            }
            Some(previous) if previous != signature => {
                change.modified.insert(path.clone());
            }
            _ => {}
        }
    }
    for path in baseline.keys() {
        if current.contains_key(path) {
            continue;
        }
        if !existing_roots.iter().any(|root| path_is_within(path, root)) {
            continue;
        }
        if root_errors.iter().any(|root| path_is_within(path, root)) {
            continue;
        }
        change.removed.insert(path.clone());
    }
    change
}
