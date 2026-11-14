use std::collections::HashMap;
use std::collections::VecDeque;

use crate::graph::Edge;
use crate::model::{normalize_text, Library, Track};
use crate::rng::Rng;

pub const AUDIO_VECTOR_FIELDS: [(&str, f64); 19] = [
    ("bpm", 1.0),
    ("loudness_db", 1.0),
    ("rms_db", 0.6),
    ("spectral_centroid", 0.9),
    ("spectral_rolloff", 0.7),
    ("zcr", 0.9),
    ("spectral_flux", 0.8),
    ("flatness_db", 0.5),
    ("hfc", 0.5),
    ("onset_rate", 0.8),
    ("dance_norm", 1.0),
    ("energy", 1.3),
    ("valence", 0.8),
    ("arousal", 1.1),
    ("dynamic_complexity", 0.5),
    ("key_strength", 0.3),
    ("tempo_norm", 1.0),
    ("brightness", 0.8),
    ("beat_confidence", 0.3),
];

#[derive(Clone, Debug, Default)]
struct MetaFeatures {
    artist: String,
    album: String,
    genre_family: String,
    genre: String,
    audio_family: String,
    mood: String,
    year: Option<i32>,
    key: String,
    scale: String,
    title: String,
}

impl MetaFeatures {
    fn from_track(track: &Track) -> Self {
        Self {
            artist: normalize_text(&track.artist),
            album: normalize_text(track.album.as_deref().unwrap_or("")),
            genre_family: normalize_text(&track.genre_family()),
            genre: normalize_text(track.display_genre()),
            audio_family: normalize_text(track.audio_genre_family()),
            mood: normalize_text(track.mood()),
            year: track.year,
            key: normalize_text(track.key()),
            scale: normalize_text(track.scale()),
            title: normalize_text(&track.title),
        }
    }
}

fn metadata_similarity(left: &MetaFeatures, right: &MetaFeatures) -> f64 {
    let mut score: f64 = 0.0;

    if !left.artist.is_empty() && left.artist == right.artist && left.artist != "unknown" {
        score += 0.32;
    }
    if !left.album.is_empty() && left.album == right.album && left.album != "unknown" {
        score += 0.30;
    }
    if !left.genre_family.is_empty()
        && left.genre_family == right.genre_family
        && left.genre_family != "unknown"
    {
        score += 0.22;
        if left.genre == right.genre {
            score += 0.08;
        }
    }
    if !left.audio_family.is_empty()
        && left.audio_family == right.audio_family
        && left.audio_family != "unknown"
    {
        score += 0.05;
    }
    if !left.mood.is_empty() && left.mood == right.mood {
        score += 0.06;
    }
    if let (Some(left_year), Some(right_year)) = (left.year, right.year) {
        if left_year == right_year {
            score += 0.04;
        } else if (left_year - right_year).abs() <= 5 {
            score += 0.02;
        }
    }
    if !left.key.is_empty() && left.key == right.key {
        score += 0.03;
        if left.scale == right.scale {
            score += 0.02;
        }
    }
    if !left.title.is_empty() && left.title == right.title {
        score += 0.05;
    }

    score.min(1.0)
}

fn audio_matrix(tracks: &[&Track]) -> Vec<Vec<f64>> {
    let mut rows: Vec<Vec<f64>> = tracks
        .iter()
        .map(|track| {
            AUDIO_VECTOR_FIELDS
                .iter()
                .map(|(field, _)| track.analysis_f64(field))
                .collect()
        })
        .collect();
    let dimensions = AUDIO_VECTOR_FIELDS.len();
    if rows.is_empty() {
        return rows;
    }
    for dimension in 0..dimensions {
        let mean = rows.iter().map(|row| row[dimension]).sum::<f64>() / rows.len() as f64;
        let variance = rows
            .iter()
            .map(|row| {
                let delta = row[dimension] - mean;
                delta * delta
            })
            .sum::<f64>()
            / rows.len() as f64;
        let std = if variance.sqrt() < 1e-9 {
            1.0
        } else {
            variance.sqrt()
        };
        let weight = AUDIO_VECTOR_FIELDS[dimension].1;
        for row in &mut rows {
            row[dimension] = (row[dimension] - mean) / std * weight;
        }
    }
    for row in &mut rows {
        let norm = row.iter().map(|value| value * value).sum::<f64>().sqrt();
        if norm > 1e-9 {
            for value in row {
                *value /= norm;
            }
        }
    }
    rows
}

fn dot(left: &[f64], right: &[f64]) -> f64 {
    left.iter().zip(right.iter()).map(|(a, b)| a * b).sum()
}

/// Direct feature similarity for one seed track. Unlike graph traversal this
/// compares every analyzed track with the seed.
pub fn feature_similarity_scores(
    library: &Library,
    seed_id: &str,
    metadata_weight: f64,
) -> Vec<(String, f64)> {
    let tracks: Vec<&Track> = library
        .tracks
        .iter()
        .filter(|track| track.has_analysis())
        .collect();
    if tracks.len() < 2 {
        return Vec::new();
    }
    let Some(seed_index) = tracks.iter().position(|track| track.id == seed_id) else {
        return Vec::new();
    };
    let matrix = audio_matrix(&tracks);
    let seed_vector = &matrix[seed_index];
    let metadata: Vec<MetaFeatures> = tracks
        .iter()
        .map(|track| MetaFeatures::from_track(track))
        .collect();
    let mut scores = Vec::with_capacity(tracks.len().saturating_sub(1));
    for (index, track) in tracks.iter().enumerate() {
        if index == seed_index {
            continue;
        }
        let audio = ((dot(&matrix[index], seed_vector) + 1.0) / 2.0).clamp(0.0, 1.0);
        let meta = metadata_similarity(&metadata[seed_index], &metadata[index]);
        let score = ((1.0 - metadata_weight) * audio + metadata_weight * meta).clamp(0.0, 1.0);
        scores.push((track.id.clone(), score));
    }
    scores
}

#[derive(Clone, Debug, Default)]
pub struct GraphStats {
    pub tracks: usize,
    pub edges: usize,
    pub average_weight: f64,
}

/// Builds the same kind of graph as the Python player: top-6 neighbours by
/// combined audio/metadata similarity, plus explicit album and artist edges.
pub fn build_similarity_graph(
    library: &Library,
    top_k: usize,
    metadata_weight: f64,
) -> (Vec<Edge>, GraphStats) {
    let tracks: Vec<&Track> = library
        .tracks
        .iter()
        .filter(|track| track.has_analysis())
        .collect();
    let mut stats = GraphStats {
        tracks: tracks.len(),
        ..Default::default()
    };
    if tracks.len() < 2 {
        return (Vec::new(), stats);
    }
    let matrix = audio_matrix(&tracks);
    if matrix.is_empty() {
        return (Vec::new(), stats);
    }
    let n = tracks.len();
    let metadata: Vec<MetaFeatures> = tracks
        .iter()
        .map(|track| MetaFeatures::from_track(track))
        .collect();
    let mut combined = vec![vec![0.0_f64; n]; n];
    for i in 0..n {
        for j in 0..n {
            let audio = ((dot(&matrix[i], &matrix[j]) + 1.0) / 2.0).clamp(0.0, 1.0);
            let meta = metadata_similarity(&metadata[i], &metadata[j]);
            combined[i][j] = (1.0 - metadata_weight) * audio + metadata_weight * meta;
        }
    }

    let mut edges: HashMap<(usize, usize), f64> = HashMap::new();
    for i in 0..n {
        let mut order: Vec<(usize, f64)> = (0..n)
            .filter(|j| *j != i)
            .map(|j| (j, combined[i][j]))
            .collect();
        order.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (j, weight) in order.into_iter().take(top_k) {
            let key = if i < j { (i, j) } else { (j, i) };
            let entry = edges.entry(key).or_insert(weight);
            if weight > *entry {
                *entry = weight;
            }
        }
    }

    for i in 0..n {
        for j in (i + 1)..n {
            let same_album = !metadata[i].album.is_empty()
                && metadata[i].album != "unknown"
                && metadata[i].album == metadata[j].album;
            let same_artist = !metadata[i].artist.is_empty()
                && metadata[i].artist != "unknown"
                && metadata[i].artist == metadata[j].artist;
            if !same_album && !same_artist {
                continue;
            }
            let mut weight = combined[i][j];
            if same_album {
                weight = weight.max(0.82);
            } else {
                weight = weight.max(0.62);
            }
            let entry = edges.entry((i, j)).or_insert(weight);
            if weight > *entry {
                *entry = weight;
            }
        }
    }

    let mut output: Vec<Edge> = edges
        .into_iter()
        .map(|((left, right), weight)| Edge {
            from: tracks[left].id.clone(),
            to: tracks[right].id.clone(),
            weight,
        })
        .collect();
    output.sort_by(|left, right| {
        let left_key = crate::paths::id_sort_key(&left.from);
        let right_key = crate::paths::id_sort_key(&right.from);
        left_key
            .cmp(&right_key)
            .then_with(|| left.to.cmp(&right.to))
    });
    stats.edges = output.len();
    if !output.is_empty() {
        stats.average_weight =
            output.iter().map(|edge| edge.weight).sum::<f64>() / output.len() as f64;
    }
    (output, stats)
}

pub fn bfs_recommendation_scores(
    graph: &crate::graph::Graph,
    seed_id: &str,
    max_depth: usize,
    limit: usize,
) -> Vec<(String, f64)> {
    if !graph.has_node(seed_id) {
        return Vec::new();
    }
    let mut scores: HashMap<String, f64> = HashMap::new();
    let mut queue: VecDeque<(String, usize, f64)> = VecDeque::new();
    queue.push_back((seed_id.to_string(), 0, 1.0));
    while let Some((node, depth, score)) = queue.pop_front() {
        if depth >= max_depth {
            continue;
        }
        for neighbor in graph.get_neighbors(&node) {
            if neighbor == seed_id {
                continue;
            }
            let Some(weight) = graph.get_edge_weight(&node, neighbor) else {
                continue;
            };
            if weight <= 0.0 {
                continue;
            }
            let new_score = score * weight * 0.65;
            if new_score <= scores.get(neighbor).copied().unwrap_or(0.0) + 1e-9 {
                continue;
            }
            scores.insert(neighbor.clone(), new_score);
            queue.push_back((neighbor.clone(), depth + 1, new_score));
        }
    }
    let mut output: Vec<(String, f64)> = scores.into_iter().collect();
    output.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    output.truncate(limit);
    output
}

pub fn recommendations(library: &Library, seed_id: &str, limit: usize) -> Vec<String> {
    let mut ids: Vec<String> = bfs_recommendation_scores(&library.graph, seed_id, 2, limit)
        .into_iter()
        .map(|(id, _)| id)
        .filter(|id| library.contains(id))
        .collect();
    if ids.is_empty()
        && library
            .find(seed_id)
            .map(Track::has_analysis)
            .unwrap_or(false)
    {
        let mut scores = feature_similarity_scores(library, seed_id, 0.3);
        scores.retain(|(id, _)| library.find(id).map(Track::playable).unwrap_or(false));
        scores.sort_by(|left, right| {
            right
                .1
                .partial_cmp(&left.1)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        ids = scores.into_iter().take(limit).map(|(id, _)| id).collect();
    }
    ids
}

pub fn radio_track_ids(
    library: &Library,
    seed_id: &str,
    limit: usize,
    rng: &mut Rng,
) -> Vec<String> {
    let Some(seed_track) = library.find(seed_id) else {
        return Vec::new();
    };
    if !seed_track.playable() {
        return Vec::new();
    }
    let limit = limit.clamp(1, 200);
    if limit == 1 {
        return vec![seed_id.to_string()];
    }

    let mut scores = feature_similarity_scores(library, seed_id, 0.3);
    if scores.is_empty() {
        scores = bfs_recommendation_scores(&library.graph, seed_id, 2, (limit * 4).max(80));
    }

    let mut candidates: Vec<(String, f64)> = scores
        .into_iter()
        .filter(|(id, _)| id != seed_id)
        .filter(|(id, _)| library.find(id).map(Track::playable).unwrap_or(false))
        .map(|(id, score)| (id, score.max(0.0)))
        .collect();

    if candidates.is_empty() {
        let mut extras: Vec<String> = library
            .playable_ids_sorted()
            .into_iter()
            .filter(|id| id != seed_id)
            .collect();
        rng.shuffle(&mut extras);
        extras.truncate(limit - 1);
        let mut output = vec![seed_id.to_string()];
        output.extend(extras);
        return output;
    }

    candidates.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let top_score = candidates[0].1;
    if top_score > 0.0 {
        let cutoff = top_score * 0.15;
        let filtered: Vec<(String, f64)> = candidates
            .iter()
            .filter(|(_, score)| *score >= cutoff)
            .cloned()
            .collect();
        if !filtered.is_empty() {
            candidates = filtered;
        }
    }

    let pool_size = (limit * 4).max(80);
    candidates.truncate(pool_size);
    let mut pool = candidates;
    let mut chosen: Vec<String> = Vec::new();
    while !pool.is_empty() && chosen.len() < limit - 1 {
        let current_top = pool
            .iter()
            .map(|(_, score)| *score)
            .fold(0.0_f64, f64::max)
            .max(1e-9);
        let weights: Vec<f64> = pool
            .iter()
            .map(|(_, score)| {
                let normalized = (score.max(0.0) / current_top).powf(0.6) + 0.04;
                normalized * rng.range(0.75, 1.25)
            })
            .collect();
        let total: f64 = weights.iter().sum();
        let mut target = rng.range(0.0, total.max(1e-9));
        let mut picked = weights.len() - 1;
        for (index, weight) in weights.iter().enumerate() {
            target -= weight;
            if target <= 0.0 {
                picked = index;
                break;
            }
        }
        chosen.push(pool.remove(picked).0);
    }

    if chosen.len() < limit - 1 {
        let used: std::collections::HashSet<&str> = std::iter::once(seed_id)
            .chain(chosen.iter().map(String::as_str))
            .collect();
        let mut extras: Vec<String> = library
            .playable_ids_sorted()
            .into_iter()
            .filter(|id| !used.contains(id.as_str()))
            .collect();
        rng.shuffle(&mut extras);
        let needed = limit - 1 - chosen.len();
        chosen.extend(extras.into_iter().take(needed));
    }

    let mut output = vec![seed_id.to_string()];
    output.extend(chosen);
    output
}
