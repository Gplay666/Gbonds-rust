//! Local audio analysis.
//!
//! The Python fork used Essentia. This implementation decodes the file with
//! ffmpeg and computes a comparable feature set in pure Rust: spectral shape,
//! MFCC, chroma, tempo, key, energy and mood. Field names match the Python
//! cache so both players can share one graph.

use std::f32::consts::PI;
use std::path::Path;
use std::process::Command;

use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use serde_json::{json, Map, Value};

use crate::model::{normalize_genre, Analysis, Track};

pub const ANALYSIS_VERSION: u32 = 1;
const SAMPLE_RATE: usize = 22_050;
const FRAME_SIZE: usize = 2048;
const HOP_SIZE: usize = 1024;
const BINS: usize = FRAME_SIZE / 2;
const MEL_FILTERS: usize = 26;
const MFCC_COUNT: usize = 13;

#[derive(Clone, Debug)]
pub struct AnalysisError(pub String);

impl std::fmt::Display for AnalysisError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for AnalysisError {}

fn decode(path: &Path) -> Result<Vec<f32>, AnalysisError> {
    if !path.is_file() {
        return Err(AnalysisError(format!("File not found: {}", path.display())));
    }
    let output = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-i"])
        .arg(path)
        .args([
            "-f",
            "f32le",
            "-ac",
            "1",
            "-ar",
            &SAMPLE_RATE.to_string(),
            "-",
        ])
        .output()
        .map_err(|error| {
            AnalysisError(format!("Cannot run ffmpeg (is FFmpeg installed?): {error}"))
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AnalysisError(format!(
            "ffmpeg could not decode {}: {}",
            path.display(),
            stderr.lines().next().unwrap_or("unknown error")
        )));
    }
    let mut samples = Vec::with_capacity(output.stdout.len() / 4);
    for chunk in output.stdout.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if value.is_finite() {
            samples.push(value);
        }
    }
    Ok(samples)
}

fn mel(frequency: f32) -> f32 {
    2595.0 * (1.0 + frequency / 700.0).log10()
}

fn inv_mel(value: f32) -> f32 {
    700.0 * (10_f32.powf(value / 2595.0) - 1.0)
}

fn mel_filterbank() -> Vec<Vec<(usize, f32)>> {
    let mut filters = Vec::with_capacity(MEL_FILTERS);
    let min_mel = mel(0.0);
    let max_mel = mel(SAMPLE_RATE as f32 / 2.0);
    let points: Vec<f32> = (0..=MEL_FILTERS + 1)
        .map(|index| min_mel + (max_mel - min_mel) * index as f32 / (MEL_FILTERS + 1) as f32)
        .collect();
    let frequencies: Vec<f32> = points.into_iter().map(inv_mel).collect();
    let bin_hz = SAMPLE_RATE as f32 / FRAME_SIZE as f32;
    for filter in 1..=MEL_FILTERS {
        let left = frequencies[filter - 1];
        let center = frequencies[filter];
        let right = frequencies[filter + 1];
        let start = ((left / bin_hz).ceil() as usize).max(1);
        let end = ((right / bin_hz).floor() as usize).min(BINS - 1);
        let mut weights = Vec::new();
        for bin in start..=end {
            let frequency = bin as f32 * bin_hz;
            let weight = if frequency <= center {
                (frequency - left) / (center - left).max(1e-9)
            } else {
                (right - frequency) / (right - center).max(1e-9)
            };
            if weight > 0.0 {
                weights.push((bin, weight));
            }
        }
        filters.push(weights);
    }
    filters
}

fn dct_matrix() -> Vec<Vec<f32>> {
    (0..MFCC_COUNT)
        .map(|coefficient| {
            (0..MEL_FILTERS)
                .map(|filter| {
                    (PI * coefficient as f32 * (filter as f32 + 0.5) / MEL_FILTERS as f32).cos()
                })
                .collect()
        })
        .collect()
}

fn chroma_map() -> Vec<Option<usize>> {
    let bin_hz = SAMPLE_RATE as f32 / FRAME_SIZE as f32;
    (0..BINS)
        .map(|bin| {
            let frequency = bin as f32 * bin_hz;
            if !(50.0..=5000.0).contains(&frequency) {
                return None;
            }
            let midi = 69.0 + 12.0 * (frequency / 440.0).log2();
            Some(((midi.round() as i32).rem_euclid(12)) as usize)
        })
        .collect()
}

fn clamp(value: f64, low: f64, high: f64) -> f64 {
    value.max(low).min(high)
}

fn infer_mood(valence: f64, arousal: f64) -> (&'static str, &'static str) {
    if arousal >= 0.55 && valence >= 0.55 {
        ("весёлое", "happy")
    } else if arousal >= 0.55 {
        ("агрессивное", "aggressive")
    } else if valence >= 0.55 {
        ("спокойное", "calm")
    } else {
        ("грустное", "sad")
    }
}

fn infer_audio_genre(
    bpm: f64,
    energy: f64,
    dance: f64,
    zcr: f64,
    brightness: f64,
    loudness: f64,
    percussiveness: f64,
) -> (&'static str, &'static str) {
    if bpm <= 0.0 {
        return ("Unknown", "Unknown");
    }
    if energy < 0.40 && bpm < 105.0 && brightness < 0.35 {
        return ("Ambient/Classical", "Classical");
    }
    if energy < 0.52 && bpm < 110.0 && zcr < 0.065 {
        return ("Jazz/Blues", "Jazz");
    }
    if energy >= 0.63 && zcr >= 0.10 && percussiveness >= 0.40 {
        return ("Punk/Metal", "Rock");
    }
    if (70.0..=105.0).contains(&bpm)
        && dance >= 0.70
        && energy < 0.60
        && zcr < 0.08
        && percussiveness < 0.55
    {
        return ("Hip-Hop/R&B", "Hip-Hop");
    }
    if dance >= 0.72 && bpm < 130.0 && brightness >= 0.35 && zcr < 0.10 {
        return ("Pop/Dance", "Pop");
    }
    if bpm >= 145.0 && energy >= 0.64 && (zcr >= 0.06 || percussiveness >= 0.50) {
        return ("Punk/Metal", "Rock");
    }
    if energy >= 0.68 || loudness >= 0.62 {
        return ("Rock", "Rock");
    }
    if bpm >= 120.0 && zcr < 0.06 && brightness < 0.40 {
        return ("Folk/Rock", "Folk");
    }
    if dance >= 0.65 && brightness >= 0.50 && zcr < 0.08 {
        return ("Pop", "Pop");
    }
    ("Rock", "Rock")
}

fn estimate_tempo(onset: &[f64]) -> (f64, f64, usize) {
    let frame_rate = SAMPLE_RATE as f64 / HOP_SIZE as f64;
    if onset.len() < 8 {
        return (0.0, 0.0, 0);
    }
    let mean = onset.iter().map(|value| *value as f64).sum::<f64>() / onset.len() as f64;
    let centered: Vec<f64> = onset
        .iter()
        .map(|value| (*value as f64 - mean).max(0.0))
        .collect();
    let mut smoothed = vec![0.0_f64; centered.len()];
    for index in 0..centered.len() {
        let left = centered[index.saturating_sub(1)];
        let right = centered[(index + 1).min(centered.len() - 1)];
        smoothed[index] = 0.25 * left + 0.5 * centered[index] + 0.25 * right;
    }
    let energy = smoothed.iter().map(|value| value * value).sum::<f64>();
    if energy < 1e-12 {
        return (0.0, 0.0, 0);
    }

    let max_lag = 30.min(smoothed.len() / 2);
    let mut acf = vec![0.0_f64; max_lag + 2];
    for lag in 1..=max_lag + 1 {
        let mut sum = 0.0;
        for index in lag..smoothed.len() {
            sum += smoothed[index] * smoothed[index - lag];
        }
        acf[lag] = sum / (smoothed.len() - lag) as f64;
    }
    let zero = acf[0].max(1e-12);

    let mut best_lag = 0;
    let mut best_score = 0.0;
    let mut best_acf = 0.0;
    for lag in 5..=max_lag {
        let bpm = 60.0 * frame_rate / lag as f64;
        if !(50.0..=220.0).contains(&bpm) {
            continue;
        }
        let mut score = acf[lag];
        if lag * 2 <= max_lag {
            score += 0.5 * acf[lag * 2];
        }
        if lag * 3 <= max_lag {
            score += 0.25 * acf[lag * 3];
        }
        let preference = (-0.5 * ((bpm / 110.0).ln() / 0.55).powi(2)).exp();
        score *= 0.85 + 0.15 * preference;
        if score > best_score {
            best_score = score;
            best_lag = lag;
            best_acf = acf[lag];
        }
    }
    if best_lag == 0 {
        return (0.0, 0.0, 0);
    }

    let mut refined = best_lag as f64;
    if best_lag > 1 && best_lag + 1 <= max_lag + 1 {
        let left = acf[best_lag - 1];
        let center = acf[best_lag];
        let right = acf[best_lag + 1];
        let denominator = left - 2.0 * center + right;
        if denominator.abs() > 1e-12 {
            let delta = 0.5 * (left - right) / denominator;
            if delta.abs() < 0.5 {
                refined += delta;
            }
        }
    }
    let mut bpm = 60.0 * frame_rate / refined.max(0.5);
    while bpm > 0.0 && bpm < 60.0 {
        bpm *= 2.0;
    }
    while bpm > 210.0 {
        bpm /= 2.0;
    }
    let confidence = clamp(best_acf / zero * 1.8, 0.0, 1.0);
    (bpm, confidence, best_lag)
}

fn estimate_key(chroma: &[f64; 12]) -> (String, &'static str, f64) {
    const MAJOR: [f64; 12] = [
        6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
    ];
    const MINOR: [f64; 12] = [
        6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
    ];
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];

    let sum = chroma.iter().sum::<f64>();
    if sum <= 1e-12 {
        return (String::new(), "", 0.0);
    }
    let normalized: Vec<f64> = chroma.iter().map(|value| value / sum).collect();
    let mean = normalized.iter().sum::<f64>() / 12.0;
    let centered: Vec<f64> = normalized.iter().map(|value| value - mean).collect();
    let centered_energy = centered
        .iter()
        .map(|value| value * value)
        .sum::<f64>()
        .sqrt();

    let mut best = (0usize, 0.0_f64, "major");
    let mut second = 0.0_f64;
    for root in 0..12 {
        for (scale, profile) in [("major", &MAJOR), ("minor", &MINOR)] {
            let rotated: Vec<f64> = (0..12)
                .map(|index| profile[(index + 12 - root) % 12])
                .collect();
            let profile_mean = rotated.iter().sum::<f64>() / 12.0;
            let profile_centered: Vec<f64> =
                rotated.iter().map(|value| value - profile_mean).collect();
            let profile_energy = profile_centered
                .iter()
                .map(|value| value * value)
                .sum::<f64>()
                .sqrt();
            let dot = centered
                .iter()
                .zip(profile_centered.iter())
                .map(|(left, right)| left * right)
                .sum::<f64>();
            let correlation = dot / (centered_energy * profile_energy).max(1e-12);
            if correlation > best.1 {
                second = best.1;
                best = (root, correlation, scale);
            } else if correlation > second {
                second = correlation;
            }
        }
    }
    let strength = clamp(
        best.1.max(0.0) * 0.65 + (best.1 - second).max(0.0) * 2.0,
        0.0,
        1.0,
    );
    (NAMES[best.0].to_string(), best.2, strength)
}

fn feature_map(samples: &[f32]) -> Analysis {
    let duration = samples.len() as f64 / SAMPLE_RATE as f64;
    let frame_count = if samples.len() >= FRAME_SIZE {
        1 + (samples.len() - FRAME_SIZE) / HOP_SIZE
    } else {
        0
    };
    if frame_count == 0 {
        return Map::new();
    }

    let window: Vec<f32> = (0..FRAME_SIZE)
        .map(|index| 0.5 - 0.5 * (2.0 * PI * index as f32 / (FRAME_SIZE as f32 - 1.0)).cos())
        .collect();
    let filters = mel_filterbank();
    let dct = dct_matrix();
    let chroma_map = chroma_map();

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME_SIZE);
    let mut buffer = vec![Complex::new(0.0_f32, 0.0_f32); FRAME_SIZE];
    let mut magnitudes = vec![0.0_f32; BINS];
    let mut previous = vec![0.0_f32; BINS];
    let mut flux_values = Vec::with_capacity(frame_count);
    let mut rms_values = Vec::with_capacity(frame_count);

    let mut centroid_sum = 0.0_f64;
    let mut centroid_sq = 0.0_f64;
    let mut rolloff_sum = 0.0_f64;
    let mut zcr_sum = 0.0_f64;
    let mut zcr_sq = 0.0_f64;
    let mut flux_sum = 0.0_f64;
    let mut flatness_sum = 0.0_f64;
    let mut hfc_sum = 0.0_f64;
    let mut mfcc_sum = [0.0_f64; MFCC_COUNT];
    let mut mfcc_sq = [0.0_f64; MFCC_COUNT];
    let mut chroma_sum = [0.0_f64; 12];
    let bin_hz = SAMPLE_RATE as f32 / FRAME_SIZE as f32;
    let mut mel_energies = vec![0.0_f32; MEL_FILTERS];

    for frame_index in 0..frame_count {
        let start = frame_index * HOP_SIZE;
        let frame = &samples[start..start + FRAME_SIZE];

        let mut rms = 0.0_f32;
        let mut zero_crossings = 0usize;
        let mut last_sign = 0i8;
        for (index, sample) in frame.iter().enumerate() {
            buffer[index] = Complex::new(sample * window[index], 0.0);
            rms += sample * sample;
            let sign = if *sample > 0.0 {
                1
            } else if *sample < 0.0 {
                -1
            } else {
                0
            };
            if sign != 0 {
                if last_sign != 0 && sign != last_sign {
                    zero_crossings += 1;
                }
                last_sign = sign;
            }
        }
        let rms = (rms / FRAME_SIZE as f32).sqrt();
        let zcr = zero_crossings as f64 / FRAME_SIZE as f64;
        zcr_sum += zcr;
        zcr_sq += zcr * zcr;
        rms_values.push(rms as f64);

        fft.process(&mut buffer);
        for index in 0..BINS {
            magnitudes[index] = buffer[index].norm();
        }

        let mut total_magnitude = 0.0_f64;
        let mut weighted_frequency = 0.0_f64;
        let mut hfc = 0.0_f64;
        let mut log_sum = 0.0_f64;
        let mut power_sum = 0.0_f64;
        for (index, magnitude) in magnitudes.iter().enumerate() {
            let value = *magnitude as f64;
            total_magnitude += value;
            weighted_frequency += value * index as f64 * bin_hz as f64;
            hfc += value * index as f64;
            let power = value * value + 1e-12;
            log_sum += power.ln();
            power_sum += power;
        }
        let centroid = if total_magnitude > 1e-12 {
            weighted_frequency / total_magnitude
        } else {
            0.0
        };
        centroid_sum += centroid;
        centroid_sq += centroid * centroid;

        let mut cumulative = 0.0_f64;
        let mut rolloff = 0.0_f64;
        if total_magnitude > 1e-12 {
            let target = total_magnitude * 0.85;
            for (index, magnitude) in magnitudes.iter().enumerate() {
                cumulative += *magnitude as f64;
                if cumulative >= target {
                    rolloff = index as f64 * bin_hz as f64;
                    break;
                }
            }
        }
        rolloff_sum += rolloff;

        let flatness_ratio = if power_sum > 1e-12 {
            (log_sum / BINS as f64).exp() / (power_sum / BINS as f64)
        } else {
            0.0
        };
        let flatness = if flatness_ratio > 1e-12 {
            10.0 * flatness_ratio.log10()
        } else {
            -120.0
        };
        flatness_sum += flatness;
        hfc_sum += hfc / BINS as f64;

        let mut flux = 0.0_f64;
        for index in 0..BINS {
            let delta = magnitudes[index] - previous[index];
            if delta > 0.0 {
                flux += delta as f64;
            }
        }
        flux /= BINS as f64;
        flux_sum += flux;
        flux_values.push(flux);
        previous.copy_from_slice(&magnitudes);

        mel_energies.fill(0.0);
        for (filter_index, filter) in filters.iter().enumerate() {
            let mut energy = 0.0_f32;
            for (bin, weight) in filter {
                let magnitude = magnitudes[*bin];
                energy += magnitude * magnitude * weight;
            }
            mel_energies[filter_index] = energy;
        }
        for (coefficient, weights) in dct.iter().enumerate() {
            let mut value = 0.0_f64;
            for (filter_index, weight) in weights.iter().enumerate() {
                let energy = mel_energies[filter_index].max(1e-10) as f64;
                value += energy.ln() * *weight as f64;
            }
            mfcc_sum[coefficient] += value;
            mfcc_sq[coefficient] += value * value;
        }

        let mut chroma = [0.0_f64; 12];
        for (index, magnitude) in magnitudes.iter().enumerate() {
            if let Some(pitch_class) = chroma_map[index] {
                chroma[pitch_class] += *magnitude as f64;
            }
        }
        let chroma_max = chroma.iter().cloned().fold(0.0_f64, f64::max);
        if chroma_max > 1e-12 {
            for value in &mut chroma {
                *value /= chroma_max;
            }
        }
        for index in 0..12 {
            chroma_sum[index] += chroma[index];
        }
    }

    let frames = frame_count as f64;
    let centroid = centroid_sum / frames;
    let centroid_std = (centroid_sq / frames - centroid * centroid).max(0.0).sqrt();
    let rolloff = rolloff_sum / frames;
    let zcr = zcr_sum / frames;
    let zcr_std = (zcr_sq / frames - zcr * zcr).max(0.0).sqrt();
    let flux = flux_sum / frames;
    let flatness_db = flatness_sum / frames;
    let hfc = hfc_sum / frames;

    let mfcc_mean: Vec<f64> = mfcc_sum.iter().map(|value| value / frames).collect();
    let mfcc_std: Vec<f64> = (0..MFCC_COUNT)
        .map(|index| {
            let mean = mfcc_sum[index] / frames;
            (mfcc_sq[index] / frames - mean * mean).max(0.0).sqrt()
        })
        .collect();
    let chroma_mean: Vec<f64> = chroma_sum.iter().map(|value| value / frames).collect();
    let mut chroma_array = [0.0_f64; 12];
    for index in 0..12 {
        chroma_array[index] = chroma_mean[index];
    }

    let (bpm, beat_confidence, _) = estimate_tempo(&flux_values);
    let (key, scale, key_strength) = estimate_key(&chroma_array);

    let rms_mean = rms_values.iter().sum::<f64>() / frames;
    let rms_variance = rms_values
        .iter()
        .map(|value| {
            let delta = value - rms_mean;
            delta * delta
        })
        .sum::<f64>()
        / frames;
    let rms_std = rms_variance.sqrt();
    let rms_db = (20.0 * rms_mean.max(1e-9).log10()).max(-80.0);
    let loudness_db = rms_db;

    let mut sorted_rms_db: Vec<f64> = rms_values
        .iter()
        .map(|value| (20.0 * value.max(1e-9).log10()).max(-80.0))
        .collect();
    sorted_rms_db
        .sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let percentile = |percent: f64| -> f64 {
        if sorted_rms_db.is_empty() {
            return 0.0;
        }
        let index = ((sorted_rms_db.len() - 1) as f64 * percent).round() as usize;
        sorted_rms_db[index.min(sorted_rms_db.len() - 1)]
    };
    let rms_db_std = {
        let mean = sorted_rms_db.iter().sum::<f64>() / sorted_rms_db.len() as f64;
        (sorted_rms_db
            .iter()
            .map(|value| (value - mean) * (value - mean))
            .sum::<f64>()
            / sorted_rms_db.len() as f64)
            .sqrt()
    };
    let dynamic_complexity = clamp(
        rms_db_std / 3.0 + (percentile(0.90) - percentile(0.10)) / 20.0,
        0.0,
        10.0,
    );

    let onset_mean = flux_values.iter().sum::<f64>() / frames;
    let onset_std = (flux_values
        .iter()
        .map(|value| (value - onset_mean) * (value - onset_mean))
        .sum::<f64>()
        / frames)
        .sqrt();
    let mut onsets = 0usize;
    let mut last_peak = 0usize;
    for index in 1..flux_values.len().saturating_sub(1) {
        let value = flux_values[index];
        if value > onset_mean + 0.35 * onset_std
            && value >= flux_values[index - 1]
            && value > flux_values[index + 1]
            && index.saturating_sub(last_peak) >= 3
        {
            onsets += 1;
            last_peak = index;
        }
    }
    let onset_rate = onsets as f64 / duration.max(1.0);

    let tempo_norm = clamp((bpm - 50.0) / 140.0, 0.0, 1.0);
    let loud_norm = clamp((loudness_db + 35.0) / 35.0, 0.0, 1.0);
    let rms_norm = clamp((rms_db + 45.0) / 45.0, 0.0, 1.0);
    let brightness = clamp((centroid - 400.0) / 4000.0, 0.0, 1.0);
    let percussiveness = clamp(onset_rate / 8.0, 0.0, 1.0);
    let zcr_norm = clamp(zcr / 0.25, 0.0, 1.0);
    let danceability = clamp(
        0.55 + 0.45 * beat_confidence
            + 0.25 * clamp(onset_rate / 4.0, 0.0, 1.0)
            + 0.15
                * if (85.0..=145.0).contains(&bpm) {
                    1.0
                } else {
                    0.0
                },
        0.0,
        1.5,
    );
    let dance_norm = clamp(danceability / 1.5, 0.0, 1.0);

    let energy = clamp(
        0.28 * tempo_norm
            + 0.24 * loud_norm
            + 0.16 * rms_norm
            + 0.14 * brightness
            + 0.10 * percussiveness
            + 0.08 * dance_norm,
        0.0,
        1.0,
    );
    let arousal = clamp(
        0.45 * energy
            + 0.22 * tempo_norm
            + 0.13 * percussiveness
            + 0.12 * loud_norm
            + 0.08 * zcr_norm,
        0.0,
        1.0,
    );
    let mode = if scale == "major" { 1.0 } else { 0.0 };
    let valence = clamp(
        0.32 * mode
            + 0.22 * dance_norm
            + 0.14 * brightness
            + 0.12 * key_strength
            + 0.10 * (1.0 - (tempo_norm - 0.5).abs() * 2.0)
            + 0.10 * (1.0 - zcr_norm),
        0.0,
        1.0,
    );
    let (mood, mood_en) = infer_mood(valence, arousal);
    let (audio_genre, audio_genre_family) = infer_audio_genre(
        bpm,
        energy,
        danceability,
        zcr,
        brightness,
        loud_norm,
        percussiveness,
    );

    let mut map = Map::new();
    map.insert("version".into(), json!(ANALYSIS_VERSION));
    map.insert("duration_audio".into(), json!(duration));
    map.insert("bpm".into(), json!(bpm));
    map.insert(
        "beat_count".into(),
        json!((duration * bpm / 60.0).round() as i64),
    );
    map.insert("beat_confidence".into(), json!(beat_confidence));
    map.insert("onset_rate".into(), json!(onset_rate));
    map.insert("danceability".into(), json!(danceability));
    map.insert("dance_norm".into(), json!(dance_norm));
    map.insert("dynamic_complexity".into(), json!(dynamic_complexity));
    map.insert("loudness_db".into(), json!(loudness_db));
    map.insert("rms_mean".into(), json!(rms_mean));
    map.insert("rms_std".into(), json!(rms_std));
    map.insert("rms_db".into(), json!(rms_db));
    map.insert("spectral_centroid".into(), json!(centroid));
    map.insert("spectral_centroid_std".into(), json!(centroid_std));
    map.insert("spectral_rolloff".into(), json!(rolloff));
    map.insert("zcr".into(), json!(zcr));
    map.insert("zcr_std".into(), json!(zcr_std));
    map.insert("spectral_flux".into(), json!(flux));
    map.insert("flatness_db".into(), json!(flatness_db));
    map.insert("hfc".into(), json!(hfc));
    map.insert("key".into(), json!(key));
    map.insert("scale".into(), json!(scale));
    map.insert("key_strength".into(), json!(key_strength));
    map.insert("mfcc_mean".into(), json!(mfcc_mean));
    map.insert("mfcc_std".into(), json!(mfcc_std));
    map.insert("chroma_mean".into(), json!(chroma_mean));
    map.insert("tempo_norm".into(), json!(tempo_norm));
    map.insert("loud_norm".into(), json!(loud_norm));
    map.insert("rms_norm".into(), json!(rms_norm));
    map.insert("brightness".into(), json!(brightness));
    map.insert("percussiveness".into(), json!(percussiveness));
    map.insert("zcr_norm".into(), json!(zcr_norm));
    map.insert("energy".into(), json!(energy));
    map.insert("arousal".into(), json!(arousal));
    map.insert("valence".into(), json!(valence));
    map.insert("mood".into(), json!(mood));
    map.insert("mood_en".into(), json!(mood_en));
    map.insert("audio_genre".into(), json!(audio_genre));
    map.insert("audio_genre_family".into(), json!(audio_genre_family));
    map.insert("errors".into(), json!(Vec::<String>::new()));
    map
}

pub fn analyze_file(path: &Path) -> Result<Analysis, AnalysisError> {
    let samples = decode(path)?;
    let duration = samples.len() as f64 / SAMPLE_RATE as f64;
    if duration < 2.0 {
        return Err(AnalysisError(format!(
            "Audio file is too short: {}",
            path.display()
        )));
    }
    let map = feature_map(&samples);
    if map.is_empty() {
        return Err(AnalysisError(format!(
            "Could not analyze audio: {}",
            path.display()
        )));
    }
    Ok(map)
}

/// Mirrors `apply_analysis` from the Python player: writes canonical genre
/// metadata back into the analysis map and track.
pub fn apply_analysis(track: &mut Track, mut analysis: Analysis) {
    let metadata_raw = if track.metadata_genre.trim().is_empty() {
        track.genre.clone()
    } else {
        track.metadata_genre.clone()
    };
    let (display, _family) = normalize_genre(&metadata_raw);
    let audio_genre = analysis
        .get("audio_genre")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let final_display = if display.is_empty() {
        if audio_genre.is_empty() {
            "Unknown".to_string()
        } else {
            audio_genre
        }
    } else {
        display
    };
    analysis.insert(
        "metadata_genre".into(),
        Value::String(final_display.clone()),
    );
    track.genre = final_display;

    if track.duration == 0 {
        if let Some(duration) = analysis.get("duration_audio").and_then(Value::as_f64) {
            track.duration = duration.round() as i64;
        }
    }
    // Keep the raw metadata genre for future re-analysis.
    if track.metadata_genre.trim().is_empty() {
        track.metadata_genre = metadata_raw;
    }
    track.analysis = Some(analysis);
}

pub fn analysis_version(analysis: &Analysis) -> u32 {
    analysis.get("version").and_then(Value::as_u64).unwrap_or(0) as u32
}
