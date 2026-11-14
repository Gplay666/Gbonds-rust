use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::model::Library;
use crate::paths::format_time;
use crate::rng::Rng;

#[derive(Clone, Debug)]
pub struct PlayerError(pub String);

impl std::fmt::Display for PlayerError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PlayerError {}

#[derive(Clone, Debug)]
pub struct PlayerStatus {
    pub position: f64,
    pub duration: f64,
    pub error: Option<String>,
}

impl PlayerStatus {
    pub fn position_label(&self) -> String {
        format!(
            "{} / {}",
            format_time(self.position),
            format_time(self.duration)
        )
    }
}

#[derive(Clone, Debug, Default)]
struct BackendState {
    state: String,
    position: f64,
    duration: f64,
    volume: i32,
    error: Option<String>,
}

trait Backend {
    fn name(&self) -> &str;
    fn start(&mut self) -> Result<(), PlayerError>;
    fn play(&mut self, path: &Path) -> Result<(), PlayerError>;
    fn pause(&mut self) -> Result<(), PlayerError>;
    fn resume(&mut self) -> Result<(), PlayerError>;
    fn stop(&mut self) -> Result<(), PlayerError>;
    fn seek(&mut self, position: f64) -> Result<(), PlayerError>;
    fn set_volume(&mut self, volume: i32) -> Result<(), PlayerError>;
    fn snapshot(&mut self) -> Result<BackendState, PlayerError>;
    fn shutdown(&mut self);
}

struct NullBackend {
    reason: String,
    volume: i32,
    state: String,
}

impl NullBackend {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            volume: 80,
            state: "stopped".to_string(),
        }
    }
}

impl Backend for NullBackend {
    fn name(&self) -> &str {
        "disabled"
    }

    fn start(&mut self) -> Result<(), PlayerError> {
        Ok(())
    }

    fn play(&mut self, _path: &Path) -> Result<(), PlayerError> {
        Err(PlayerError(self.reason.clone()))
    }

    fn pause(&mut self) -> Result<(), PlayerError> {
        Ok(())
    }

    fn resume(&mut self) -> Result<(), PlayerError> {
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        self.state = "stopped".to_string();
        Ok(())
    }

    fn seek(&mut self, _position: f64) -> Result<(), PlayerError> {
        Ok(())
    }

    fn set_volume(&mut self, volume: i32) -> Result<(), PlayerError> {
        self.volume = volume.clamp(0, 100);
        Ok(())
    }

    fn snapshot(&mut self) -> Result<BackendState, PlayerError> {
        Ok(BackendState {
            state: self.state.clone(),
            position: 0.0,
            duration: 0.0,
            volume: self.volume,
            error: Some(self.reason.clone()),
        })
    }

    fn shutdown(&mut self) {}
}

#[cfg(unix)]
struct MpvBackend {
    volume: i32,
    audio_output: Option<String>,
    process: Option<Child>,
    writer: Option<std::os::unix::net::UnixStream>,
    reader: Option<BufReader<std::os::unix::net::UnixStream>>,
    socket_path: Option<PathBuf>,
    request_id: u64,
    started: bool,
}

#[cfg(not(unix))]
struct MpvBackend {
    reason: String,
}

#[cfg(unix)]
impl MpvBackend {
    fn new(volume: i32, audio_output: Option<String>) -> Self {
        Self {
            volume: volume.clamp(0, 100),
            audio_output,
            process: None,
            writer: None,
            reader: None,
            socket_path: None,
            request_id: 0,
            started: false,
        }
    }

    fn request(&mut self, command: Vec<Value>) -> Result<Value, PlayerError> {
        if !self.started {
            return Err(PlayerError("mpv is not running.".to_string()));
        }
        let (Some(writer), Some(reader)) = (self.writer.as_mut(), self.reader.as_mut()) else {
            return Err(PlayerError("mpv IPC is not connected.".to_string()));
        };
        self.request_id += 1;
        let request_id = self.request_id;
        let payload = json!({
            "command": command,
            "request_id": request_id,
        });
        let mut bytes = payload.to_string().into_bytes();
        bytes.push(b'\n');
        writer
            .write_all(&bytes)
            .map_err(|error| PlayerError(format!("Cannot write to mpv: {error}")))?;

        loop {
            let mut line = String::new();
            let read = reader
                .read_line(&mut line)
                .map_err(|error| PlayerError(format!("mpv IPC did not answer: {error}")))?;
            if read == 0 {
                return Err(PlayerError("mpv closed the IPC connection.".to_string()));
            }
            let Ok(message) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if message.get("request_id").and_then(Value::as_u64) != Some(request_id) {
                continue;
            }
            if message.get("error").and_then(Value::as_str) != Some("success") {
                let error = message
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("mpv error");
                return Err(PlayerError(error.to_string()));
            }
            return Ok(message.get("data").cloned().unwrap_or(Value::Null));
        }
    }

    fn property_bool(&mut self, name: &str, default: bool) -> bool {
        self.request(vec![json!("get_property"), json!(name)])
            .ok()
            .and_then(|value| value.as_bool())
            .unwrap_or(default)
    }

    fn property_f64(&mut self, name: &str, default: f64) -> f64 {
        self.request(vec![json!("get_property"), json!(name)])
            .ok()
            .and_then(|value| value.as_f64())
            .unwrap_or(default)
    }

    fn property_i64(&mut self, name: &str, default: i64) -> i64 {
        self.request(vec![json!("get_property"), json!(name)])
            .ok()
            .and_then(|value| value.as_i64())
            .unwrap_or(default)
    }

    fn process_alive(&mut self) -> bool {
        match self.process.as_mut() {
            Some(process) => matches!(process.try_wait(), Ok(None)),
            None => false,
        }
    }
}

#[cfg(unix)]
impl Backend for MpvBackend {
    fn name(&self) -> &str {
        "mpv"
    }

    fn start(&mut self) -> Result<(), PlayerError> {
        if self.started && self.process_alive() {
            return Ok(());
        }
        self.shutdown();
        if find_in_path("mpv").is_none() {
            return Err(PlayerError(
                "mpv is not installed. Install mpv to enable playback.".to_string(),
            ));
        }

        let runtime = env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .unwrap_or_else(env::temp_dir);
        let mut socket_path = runtime.join(format!("gbonds-{}.sock", std::process::id()));
        if socket_path.to_string_lossy().len() > 100 {
            socket_path = env::temp_dir().join(format!("gbonds-{}.sock", std::process::id()));
        }
        let _ = fs::remove_file(&socket_path);

        let mut command = Command::new("mpv");
        command.args([
            "--no-video",
            "--no-terminal",
            "--really-quiet",
            "--idle=yes",
            "--keep-open=no",
            "--audio-display=no",
            "--force-window=no",
            &format!("--volume={}", self.volume),
            &format!("--input-ipc-server={}", socket_path.display()),
        ]);
        if let Some(audio_output) = &self.audio_output {
            command.arg(format!("--ao={audio_output}"));
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = command
            .spawn()
            .map_err(|error| PlayerError(format!("Cannot start mpv: {error}")))?;
        self.process = Some(child);
        self.socket_path = Some(socket_path.clone());

        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if let Some(process) = self.process.as_mut() {
                if let Ok(Some(status)) = process.try_wait() {
                    self.shutdown();
                    return Err(PlayerError(format!(
                        "mpv exited during startup ({status}). Check your audio setup."
                    )));
                }
            }
            if socket_path.exists() {
                match std::os::unix::net::UnixStream::connect(&socket_path) {
                    Ok(stream) => {
                        stream.set_read_timeout(Some(Duration::from_secs(2))).ok();
                        stream.set_write_timeout(Some(Duration::from_secs(2))).ok();
                        let reader = stream.try_clone().map(BufReader::new).map_err(|error| {
                            PlayerError(format!("Cannot clone mpv IPC socket: {error}"))
                        })?;
                        self.writer = Some(stream);
                        self.reader = Some(reader);
                        self.started = true;
                        self.set_volume(self.volume)?;
                        return Ok(());
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(30)),
                }
            } else {
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        self.shutdown();
        Err(PlayerError(
            "Could not connect to mpv IPC. Check that mpv starts correctly.".to_string(),
        ))
    }

    fn play(&mut self, path: &Path) -> Result<(), PlayerError> {
        self.start()?;
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.request(vec![
            json!("loadfile"),
            json!(path.to_string_lossy()),
            json!("replace"),
        ])?;
        self.request(vec![json!("set_property"), json!("pause"), json!(false)])?;
        self.request(vec![json!("set_property"), json!("mute"), json!(false)])?;
        self.set_volume(self.volume)?;
        Ok(())
    }

    fn pause(&mut self) -> Result<(), PlayerError> {
        self.request(vec![json!("set_property"), json!("pause"), json!(true)])?;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), PlayerError> {
        self.request(vec![json!("set_property"), json!("pause"), json!(false)])?;
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        if self.started {
            let _ = self.request(vec![json!("stop")]);
        }
        Ok(())
    }

    fn seek(&mut self, position: f64) -> Result<(), PlayerError> {
        self.request(vec![
            json!("seek"),
            json!(position.max(0.0)),
            json!("absolute"),
        ])?;
        Ok(())
    }

    fn set_volume(&mut self, volume: i32) -> Result<(), PlayerError> {
        self.volume = volume.clamp(0, 100);
        if self.started {
            let _ = self.request(vec![
                json!("set_property"),
                json!("volume"),
                json!(self.volume),
            ]);
        }
        Ok(())
    }

    fn snapshot(&mut self) -> Result<BackendState, PlayerError> {
        if !self.started {
            return Ok(BackendState {
                state: "stopped".to_string(),
                volume: self.volume,
                ..Default::default()
            });
        }
        if !self.process_alive() {
            self.started = false;
            return Ok(BackendState {
                state: "error".to_string(),
                volume: self.volume,
                error: Some("mpv exited.".to_string()),
                ..Default::default()
            });
        }
        let idle = self.property_bool("idle-active", true);
        let paused = self.property_bool("pause", false);
        let position = self.property_f64("time-pos", 0.0);
        let duration = self.property_f64("duration", 0.0);
        let volume = self.property_i64("volume", self.volume as i64) as i32;
        let state = if idle {
            "stopped"
        } else if paused {
            "paused"
        } else {
            "playing"
        };
        Ok(BackendState {
            state: state.to_string(),
            position: position.max(0.0),
            duration: duration.max(0.0),
            volume: volume.clamp(0, 100),
            error: None,
        })
    }

    fn shutdown(&mut self) {
        if self.started {
            let _ = self.request(vec![json!("quit")]);
        }
        self.reader = None;
        self.writer = None;
        self.started = false;
        if let Some(child) = self.process.as_mut() {
            let deadline = Instant::now() + Duration::from_millis(1500);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    _ => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                }
            }
        }
        self.process = None;
        if let Some(socket) = self.socket_path.take() {
            let _ = fs::remove_file(socket);
        }
    }
}

#[cfg(unix)]
impl Drop for MpvBackend {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(not(unix))]
impl Backend for MpvBackend {
    fn name(&self) -> &str {
        "unavailable"
    }
    fn start(&mut self) -> Result<(), PlayerError> {
        Err(PlayerError(self.reason.clone()))
    }
    fn play(&mut self, _path: &Path) -> Result<(), PlayerError> {
        Err(PlayerError(self.reason.clone()))
    }
    fn pause(&mut self) -> Result<(), PlayerError> {
        Ok(())
    }
    fn resume(&mut self) -> Result<(), PlayerError> {
        Ok(())
    }
    fn stop(&mut self) -> Result<(), PlayerError> {
        Ok(())
    }
    fn seek(&mut self, _position: f64) -> Result<(), PlayerError> {
        Ok(())
    }
    fn set_volume(&mut self, _volume: i32) -> Result<(), PlayerError> {
        Ok(())
    }
    fn snapshot(&mut self) -> Result<BackendState, PlayerError> {
        Ok(BackendState::default())
    }
    fn shutdown(&mut self) {}
}

enum BackendImpl {
    Mpv(MpvBackend),
    Null(NullBackend),
}

impl BackendImpl {
    fn name(&self) -> &str {
        match self {
            BackendImpl::Mpv(backend) => backend.name(),
            BackendImpl::Null(backend) => backend.name(),
        }
    }

    fn start(&mut self) -> Result<(), PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.start(),
            BackendImpl::Null(backend) => backend.start(),
        }
    }

    fn play(&mut self, path: &Path) -> Result<(), PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.play(path),
            BackendImpl::Null(backend) => backend.play(path),
        }
    }

    fn pause(&mut self) -> Result<(), PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.pause(),
            BackendImpl::Null(backend) => backend.pause(),
        }
    }

    fn resume(&mut self) -> Result<(), PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.resume(),
            BackendImpl::Null(backend) => backend.resume(),
        }
    }

    fn stop(&mut self) -> Result<(), PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.stop(),
            BackendImpl::Null(backend) => backend.stop(),
        }
    }

    fn seek(&mut self, position: f64) -> Result<(), PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.seek(position),
            BackendImpl::Null(backend) => backend.seek(position),
        }
    }

    fn set_volume(&mut self, volume: i32) -> Result<(), PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.set_volume(volume),
            BackendImpl::Null(backend) => backend.set_volume(volume),
        }
    }

    fn snapshot(&mut self) -> Result<BackendState, PlayerError> {
        match self {
            BackendImpl::Mpv(backend) => backend.snapshot(),
            BackendImpl::Null(backend) => backend.snapshot(),
        }
    }

    fn shutdown(&mut self) {
        match self {
            BackendImpl::Mpv(backend) => backend.shutdown(),
            BackendImpl::Null(backend) => backend.shutdown(),
        }
    }
}

fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(program);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn make_backend(volume: i32) -> BackendImpl {
    let name = env::var("GBONDS_BACKEND")
        .or_else(|_| env::var("TUI_PLAYER_BACKEND"))
        .unwrap_or_else(|_| "mpv".to_string())
        .to_lowercase();
    if matches!(name.as_str(), "none" | "null" | "dummy" | "off") {
        return BackendImpl::Null(NullBackend::new(
            "Audio backend disabled (GBONDS_BACKEND/TUI_PLAYER_BACKEND).",
        ));
    }
    if name != "mpv" {
        return BackendImpl::Null(NullBackend::new(format!("Unknown audio backend: {name}")));
    }
    #[cfg(unix)]
    {
        let audio_output = env::var("GBONDS_MPV_AO")
            .or_else(|_| env::var("TUI_MPV_AO"))
            .ok();
        BackendImpl::Mpv(MpvBackend::new(volume, audio_output))
    }
    #[cfg(not(unix))]
    {
        BackendImpl::Null(NullBackend::new(
            "mpv playback is supported on Unix only in this build.",
        ))
    }
}

pub struct Player {
    library: Arc<RwLock<Library>>,
    backend: BackendImpl,
    pub queue: Vec<String>,
    pub queue_index: i64,
    pub current_id: Option<String>,
    pub shuffle: bool,
    pub repeat: String,
    pub volume: i32,
    state: String,
    position: f64,
    duration: f64,
    started: bool,
    saw_playing: bool,
    idle_since: Option<Instant>,
    pub last_error: Option<String>,
    rng: Rng,
}

impl Player {
    pub fn new(library: Arc<RwLock<Library>>, volume: i32) -> Self {
        let mut rng = Rng::from_time();
        let mut queue = Vec::new();
        rng.shuffle(&mut queue);
        Self {
            library,
            backend: make_backend(volume),
            queue,
            queue_index: -1,
            current_id: None,
            shuffle: false,
            repeat: "off".to_string(),
            volume: volume.clamp(0, 100),
            state: "stopped".to_string(),
            position: 0.0,
            duration: 0.0,
            started: false,
            saw_playing: false,
            idle_since: None,
            last_error: None,
            rng,
        }
    }

    pub fn backend_name(&self) -> &str {
        self.backend.name()
    }

    pub fn status(&self) -> PlayerStatus {
        PlayerStatus {
            position: self.position,
            duration: self.duration,
            error: self.last_error.clone(),
        }
    }

    pub fn state(&self) -> &str {
        &self.state
    }

    pub fn position(&self) -> f64 {
        self.position
    }

    pub fn duration(&self) -> f64 {
        self.duration
    }

    pub fn current_track(&self) -> Option<crate::model::Track> {
        let id = self.current_id.as_ref()?;
        self.library.read().ok()?.find(id).cloned()
    }

    fn track_path(&self, id: &str) -> Result<(PathBuf, f64), PlayerError> {
        let library = self
            .library
            .read()
            .map_err(|_| PlayerError("Library lock is poisoned.".to_string()))?;
        let track = library
            .find(id)
            .ok_or_else(|| PlayerError(format!("Track {id} was not found.")))?;
        let filepath = track
            .filepath
            .as_ref()
            .ok_or_else(|| PlayerError("This track has no file path.".to_string()))?;
        let path = PathBuf::from(filepath);
        if !path.is_file() {
            return Err(PlayerError(format!("File not found: {}", path.display())));
        }
        Ok((path, track.duration_seconds()))
    }

    fn ensure_started(&mut self) -> Result<(), PlayerError> {
        if self.started {
            return Ok(());
        }
        self.backend.start()?;
        self.started = true;
        self.backend.set_volume(self.volume)?;
        Ok(())
    }

    fn play_current(&mut self) -> Result<(), PlayerError> {
        let Some(current_id) = self.current_id.clone() else {
            return Err(PlayerError("There is no current track.".to_string()));
        };
        let (path, duration) = self.track_path(&current_id)?;
        self.ensure_started()?;
        self.backend.play(&path)?;
        self.state = "playing".to_string();
        self.position = 0.0;
        self.duration = duration;
        self.saw_playing = false;
        self.idle_since = None;
        self.last_error = None;
        Ok(())
    }

    pub fn play_queue_index(&mut self, index: i64) -> Result<(), PlayerError> {
        if index < 0 || index as usize >= self.queue.len() {
            return Err(PlayerError("No such queue item.".to_string()));
        }
        self.queue_index = index;
        self.current_id = self.queue.get(index as usize).cloned();
        self.play_current()
    }

    pub fn set_queue(&mut self, ids: &[String], autoplay: bool) -> Result<(), PlayerError> {
        self.stop();
        self.queue = ids.to_vec();
        self.queue_index = if autoplay && !ids.is_empty() { 0 } else { -1 };
        self.current_id = if autoplay { ids.first().cloned() } else { None };
        if autoplay && !ids.is_empty() {
            self.play_current()?;
        }
        Ok(())
    }

    pub fn enqueue(&mut self, ids: &[String], play_next: bool) -> usize {
        if ids.is_empty() {
            return 0;
        }
        if play_next && self.current_id.is_some() {
            let position = (self.queue_index + 1).max(0) as usize;
            for (offset, id) in ids.iter().enumerate() {
                self.queue.insert(position + offset, id.clone());
            }
        } else {
            self.queue.extend_from_slice(ids);
        }
        ids.len()
    }

    pub fn enqueue_track(&mut self, id: &str, play_next: bool) -> usize {
        self.enqueue(&[id.to_string()], play_next)
    }

    pub fn remove_queue_index(&mut self, index: i64) -> Result<(), PlayerError> {
        let index = index.max(0) as usize;
        if index >= self.queue.len() {
            return Err(PlayerError("No such queue item.".to_string()));
        }
        let was_current = index as i64 == self.queue_index;
        if was_current {
            let _ = self.backend.stop();
            self.state = "stopped".to_string();
            self.saw_playing = false;
        }
        self.queue.remove(index);
        if self.queue.is_empty() {
            self.queue_index = -1;
            self.current_id = None;
            self.position = 0.0;
            self.duration = 0.0;
            return Ok(());
        }
        if was_current {
            self.queue_index = index.min(self.queue.len() - 1) as i64;
            self.current_id = self.queue.get(self.queue_index as usize).cloned();
            if let Err(error) = self.play_current() {
                self.last_error = Some(error.0);
            }
        } else if index < self.queue_index.max(0) as usize {
            self.queue_index -= 1;
        }
        Ok(())
    }

    pub fn move_queue(&mut self, from: i64, to: i64) -> Result<(), PlayerError> {
        if from < 0 || from as usize >= self.queue.len() {
            return Err(PlayerError("No such queue item.".to_string()));
        }
        let to = to.max(0).min(self.queue.len() as i64 - 1) as usize;
        let from = from as usize;
        if from == to {
            return Ok(());
        }
        let item = self.queue.remove(from);
        self.queue.insert(to, item);
        if let Some(current) = &self.current_id {
            if let Some(position) = self.queue.iter().position(|id| id == current) {
                self.queue_index = position as i64;
            }
        }
        Ok(())
    }

    pub fn remove_track(&mut self, id: &str) {
        if self.current_id.as_deref() == Some(id) {
            self.stop();
        }
        self.queue.retain(|item| item != id);
        if let Some(current) = &self.current_id {
            if let Some(position) = self.queue.iter().position(|item| item == current) {
                self.queue_index = position as i64;
            } else {
                self.current_id = None;
                self.queue_index = -1;
                self.state = "stopped".to_string();
                self.position = 0.0;
                self.duration = 0.0;
            }
        }
    }

    pub fn clear_queue(&mut self) {
        self.stop();
        self.queue.clear();
        self.queue_index = -1;
        self.current_id = None;
        self.position = 0.0;
        self.duration = 0.0;
    }

    pub fn toggle(&mut self) -> Result<(), PlayerError> {
        if self.current_id.is_none() {
            if let Some(index) = if self.queue.is_empty() {
                None
            } else {
                Some(self.queue_index.max(0))
            } {
                return self.play_queue_index(index);
            }
            return Err(PlayerError("The queue is empty.".to_string()));
        }
        if !self.started {
            return self.play_current();
        }
        let state = self.backend.snapshot()?;
        match state.state.as_str() {
            "playing" => {
                self.backend.pause()?;
                self.state = "paused".to_string();
            }
            "paused" => {
                self.backend.resume()?;
                self.state = "playing".to_string();
            }
            "error" => {
                return Err(PlayerError(
                    state
                        .error
                        .unwrap_or_else(|| "Playback backend error.".to_string()),
                ))
            }
            _ => self.play_current()?,
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        if self.started {
            let _ = self.backend.stop();
        }
        self.state = "stopped".to_string();
        self.position = 0.0;
        self.saw_playing = false;
        self.idle_since = None;
    }

    fn next_index(&mut self, automatic: bool) -> Option<usize> {
        if self.queue.is_empty() {
            return None;
        }
        if self.shuffle && self.queue.len() > 1 {
            let choices: Vec<usize> = (0..self.queue.len())
                .filter(|index| *index as i64 != self.queue_index)
                .collect();
            if choices.is_empty() {
                return None;
            }
            let choice = choices[self.rng.below(choices.len())];
            return Some(choice);
        }
        let index = self.queue_index + 1;
        if index >= 0 && (index as usize) < self.queue.len() {
            return Some(index as usize);
        }
        if automatic && self.repeat == "all" {
            return Some(0);
        }
        if !automatic && self.repeat == "all" {
            return Some(0);
        }
        None
    }

    pub fn next(&mut self) -> bool {
        let Some(index) = self.next_index(false) else {
            self.stop();
            return false;
        };
        self.queue_index = index as i64;
        self.current_id = self.queue.get(index).cloned();
        if let Err(error) = self.play_current() {
            self.last_error = Some(error.0);
            self.state = "stopped".to_string();
            return false;
        }
        true
    }

    pub fn previous(&mut self) -> bool {
        if self.position > 3.0 {
            let _ = self.seek_to(0.0);
            return true;
        }
        if self.queue.is_empty() {
            return false;
        }
        let mut index = self.queue_index - 1;
        if index < 0 {
            index = if self.repeat == "all" {
                self.queue.len() as i64 - 1
            } else {
                0
            };
        }
        self.queue_index = index;
        self.current_id = self.queue.get(index as usize).cloned();
        if let Err(error) = self.play_current() {
            self.last_error = Some(error.0);
            return false;
        }
        true
    }

    fn after_end(&mut self) {
        self.saw_playing = false;
        self.idle_since = None;
        if self.repeat == "one" && self.current_id.is_some() {
            if let Err(error) = self.play_current() {
                self.last_error = Some(error.0);
            }
            return;
        }
        let Some(index) = self.next_index(true) else {
            self.state = "stopped".to_string();
            self.position = 0.0;
            return;
        };
        self.queue_index = index as i64;
        self.current_id = self.queue.get(index).cloned();
        if let Err(error) = self.play_current() {
            self.last_error = Some(error.0);
            self.state = "stopped".to_string();
        }
    }

    pub fn tick(&mut self) {
        if !self.started || self.current_id.is_none() {
            return;
        }
        let state = match self.backend.snapshot() {
            Ok(state) => state,
            Err(error) => {
                self.last_error = Some(error.0);
                self.state = "error".to_string();
                return;
            }
        };
        if state.volume > 0 {
            self.volume = state.volume;
        }
        self.position = state.position.max(0.0);
        if state.duration > 0.0 {
            self.duration = state.duration;
        }
        self.state = state.state;
        if let Some(error) = state.error {
            self.last_error = Some(error);
        }
        match self.state.as_str() {
            "playing" => {
                self.saw_playing = true;
                self.idle_since = None;
            }
            "stopped" if self.saw_playing => {
                let now = Instant::now();
                match self.idle_since {
                    None => self.idle_since = Some(now),
                    Some(since) if now.duration_since(since) >= Duration::from_millis(700) => {
                        self.after_end();
                    }
                    _ => {}
                }
            }
            "paused" => self.idle_since = None,
            _ => {}
        }
    }

    pub fn seek_to(&mut self, position: f64) -> Result<(), PlayerError> {
        if self.current_id.is_none() {
            return Err(PlayerError("There is no current track.".to_string()));
        }
        let mut target = position.max(0.0);
        if self.duration > 0.0 {
            target = target.min((self.duration - 0.25).max(0.0));
        }
        if self.started {
            self.backend.seek(target)?;
        }
        self.position = target;
        Ok(())
    }

    pub fn seek_relative(&mut self, delta: f64) -> Result<(), PlayerError> {
        self.seek_to(self.position + delta)
    }

    pub fn set_volume(&mut self, volume: i32) {
        self.volume = volume.clamp(0, 100);
        if self.started {
            let _ = self.backend.set_volume(self.volume);
        }
    }

    pub fn change_volume(&mut self, delta: i32) {
        self.set_volume(self.volume + delta);
    }

    pub fn toggle_shuffle(&mut self) -> bool {
        self.shuffle = !self.shuffle;
        self.shuffle
    }

    pub fn cycle_repeat(&mut self) -> String {
        self.repeat = match self.repeat.as_str() {
            "off" => "all".to_string(),
            "all" => "one".to_string(),
            _ => "off".to_string(),
        };
        self.repeat.clone()
    }

    pub fn shutdown(&mut self) {
        self.backend.shutdown();
        self.started = false;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.shutdown();
    }
}
