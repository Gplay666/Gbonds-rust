mod analysis;
mod app;
mod graph;
mod metadata;
mod model;
mod paths;
mod player;
mod rng;
mod similarity;
mod store;
mod theme;
mod ui;
mod util;
mod watcher;
mod worker;

use std::env;
use std::io::{self, stdout};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event,
};
use crossterm::execute;

use crate::app::{App, APP_NAME, APP_VERSION};
use crate::paths::{display_folder, AppPaths};
use crate::store::{FolderStore, PlaylistStore};
use crate::theme::ThemeMode;
use crate::worker::{WorkerCommand, WorkerHandle, WorkerMessage};

#[derive(Clone, Debug)]
enum Action {
    Tui,
    Status,
    Scan { force_analysis: bool },
    AddFolder(String),
    Folders,
    Help,
    Version,
}

struct Cli {
    action: Action,
    data_dir: Option<PathBuf>,
    library: Option<PathBuf>,
    theme: ThemeMode,
}

fn main() -> ExitCode {
    let cli = match parse_args(env::args().skip(1).collect()) {
        Ok(cli) => cli,
        Err(error) => {
            eprintln!("gbonds: {error}");
            eprintln!("Try `gbonds --help` for usage.");
            return ExitCode::from(2);
        }
    };

    let mut paths = match &cli.data_dir {
        Some(dir) => AppPaths::from_data_dir(dir.clone()),
        None => AppPaths::discover(),
    };
    if let Some(library) = &cli.library {
        paths.library = library.clone();
    }

    match cli.action {
        Action::Help => {
            print_help();
            ExitCode::SUCCESS
        }
        Action::Version => {
            println!("{APP_NAME} {APP_VERSION}");
            ExitCode::SUCCESS
        }
        Action::Status => run_status(&paths),
        Action::Folders => run_folders(&paths),
        Action::AddFolder(folder) => run_add_folder(&paths, &folder),
        Action::Scan { force_analysis } => run_scan(&paths, force_analysis),
        Action::Tui => match run_tui(paths, cli.theme) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("gbonds: {error}");
                ExitCode::FAILURE
            }
        },
    }
}

fn parse_args(args: Vec<String>) -> Result<Cli, String> {
    let mut data_dir = None;
    let mut library = None;
    let mut theme = ThemeMode::from_env();
    let mut action: Option<Action> = None;
    let mut index = 0;

    let set_action = |slot: &mut Option<Action>, value: Action| -> Result<(), String> {
        if slot.is_some() {
            return Err("only one of --status, --scan, --analyze, --folders or --add-folder can be used at once".to_string());
        }
        *slot = Some(value);
        Ok(())
    };

    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--data-dir" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--data-dir needs a path".to_string())?;
                data_dir = Some(PathBuf::from(value));
            }
            "--library" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--library needs a path".to_string())?;
                library = Some(PathBuf::from(value));
            }
            "--theme" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--theme needs auto, dark or light".to_string())?;
                theme = ThemeMode::parse(value).ok_or_else(|| {
                    format!("unknown theme “{value}” (expected auto, dark or light)")
                })?;
            }
            "--status" => set_action(&mut action, Action::Status)?,
            "--scan" => set_action(
                &mut action,
                Action::Scan {
                    force_analysis: false,
                },
            )?,
            "--analyze" => set_action(
                &mut action,
                Action::Scan {
                    force_analysis: true,
                },
            )?,
            "--folders" => set_action(&mut action, Action::Folders)?,
            "--add-folder" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--add-folder needs a path".to_string())?;
                set_action(&mut action, Action::AddFolder(value.clone()))?;
            }
            "--help" | "-h" => {
                return Ok(Cli {
                    action: Action::Help,
                    data_dir,
                    library,
                    theme,
                })
            }
            "--version" | "-V" => {
                return Ok(Cli {
                    action: Action::Version,
                    data_dir,
                    library,
                    theme,
                })
            }
            "--" => {
                return Err(format!(
                    "unexpected arguments: {}",
                    args[index + 1..].join(" ")
                ));
            }
            other if other.starts_with('-') => {
                return Err(format!("unknown option “{other}”"));
            }
            other => {
                return Err(format!(
                    "unexpected argument “{other}”; music folders are added with --add-folder"
                ));
            }
        }
        index += 1;
    }

    Ok(Cli {
        action: action.unwrap_or(Action::Tui),
        data_dir,
        library,
        theme,
    })
}

fn print_help() {
    println!(
        "{APP_NAME} {APP_VERSION} — a terminal music player with local recommendations\n\
\n\
USAGE:\n\
    gbonds [OPTIONS]\n\
\n\
OPTIONS:\n\
    --data-dir <DIR>       use another data directory\n\
    --library <FILE>       use another library.json\n\
    --theme <MODE>         auto, dark or light (default: auto)\n\
    --status               print library and analysis status, then exit\n\
    --folders              print connected folders and playlists, then exit\n\
    --scan                 import new or changed files headlessly\n\
    --analyze              force audio analysis for every track, then exit\n\
    --add-folder <PATH>    connect a music folder (run --scan afterwards)\n\
    -h, --help             show this help\n\
    -V, --version          show the version\n\
\n\
TUI:\n\
    Run without an action to open the interface. Press ? for the key map.\n\
\n\
DATA:\n\
    By default gbonds uses ~/.local/share/gbonds. On first launch it\n\
    copies an existing TUIMusicRecomendator cache there. The Python cache\n\
    is never modified. GBONDS_DATA_DIR and GBONDS_LIBRARY override the paths."
    );
}

// ---------------------------------------------------------------------------
// Headless commands
// ---------------------------------------------------------------------------

fn run_status(paths: &AppPaths) -> ExitCode {
    println!("{APP_NAME} {APP_VERSION}");
    println!("Data:    {}", paths.data_dir.display());
    println!("Library: {}", paths.library.display());
    let (library, report) = store::load_library(&paths.library);
    println!("Tracks:  {}", library.tracks.len());
    println!("Links:   {}", library.graph.edge_count());
    if let Some(saved_at) = library.saved_at.as_deref() {
        println!("Saved:   {saved_at}");
    }
    if let Some(error) = report.error.as_deref() {
        println!("Error:   {error}");
    }

    let folders = FolderStore::load(paths.folders.clone());
    println!(
        "Folders: {} (watch {})",
        folders.folders.len(),
        if folders.watch_enabled { "on" } else { "off" }
    );
    if let Some(error) = folders.error.as_deref() {
        println!("Folders error: {error}");
    }
    for folder in &folders.folders {
        let count = library
            .tracks
            .iter()
            .filter(|track| {
                track
                    .filepath
                    .as_deref()
                    .map(|path| crate::paths::path_is_within(path, folder))
                    .unwrap_or(false)
            })
            .count();
        println!("  {}  {count} tracks", display_folder(folder));
    }

    let playlists = PlaylistStore::load(paths.playlists.clone());
    println!(
        "Playlists: {} ({} tracks)",
        playlists.names().len(),
        playlists.total_items()
    );
    println!(
        "mpv:     {}",
        if command_exists("mpv") {
            "available"
        } else {
            "NOT FOUND — playback will not work"
        }
    );
    let state = store::read_analysis_state(&paths.analysis_state);
    print!("{}", store::format_analysis_state(state.as_ref()));
    if report.error.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run_folders(paths: &AppPaths) -> ExitCode {
    let folders = FolderStore::load(paths.folders.clone());
    let (library, report) = store::load_library(&paths.library);
    let playlists = PlaylistStore::load(paths.playlists.clone());

    println!("Data: {}", paths.data_dir.display());
    println!(
        "Library: {} tracks, {} links",
        library.tracks.len(),
        library.graph.edge_count()
    );
    if let Some(error) = report.error.as_deref() {
        println!("Library error: {error}");
    }
    println!(
        "Watching: {}",
        if folders.watch_enabled { "on" } else { "off" }
    );
    if folders.folders.is_empty() {
        println!("No folders. Add one with `gbonds --add-folder PATH`.");
    } else {
        for folder in &folders.folders {
            let count = library
                .tracks
                .iter()
                .filter(|track| {
                    track
                        .filepath
                        .as_deref()
                        .map(|path| crate::paths::path_is_within(path, folder))
                        .unwrap_or(false)
                })
                .count();
            let exists = std::path::Path::new(folder).is_dir();
            println!(
                "  {}  {count} tracks{}",
                display_folder(folder),
                if exists { "" } else { "  [missing]" }
            );
        }
    }
    println!(
        "Playlists: {} ({} tracks)",
        playlists.names().len(),
        playlists.total_items()
    );
    for name in playlists.names() {
        println!("  {name}  {} tracks", playlists.count(&name));
    }
    if let Some(error) = folders.error.as_deref() {
        eprintln!("folders.json: {error}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn run_add_folder(paths: &AppPaths, folder: &str) -> ExitCode {
    let mut folders = FolderStore::load(paths.folders.clone());
    match folders.add(folder) {
        Ok(folder) => {
            if let Err(error) = folders.save() {
                eprintln!("gbonds: cannot save folders: {error}");
                return ExitCode::FAILURE;
            }
            println!("Added {}", display_folder(&folder));
            println!("Run `gbonds --scan` to import files, or open the TUI and press :scan.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("gbonds: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run_scan(paths: &AppPaths, force_analysis: bool) -> ExitCode {
    let folders = FolderStore::load(paths.folders.clone());
    if folders.folders.is_empty() {
        eprintln!("gbonds: no music folders configured; use --add-folder PATH first");
        return ExitCode::FAILURE;
    }
    let (library, report) = store::load_library(&paths.library);
    if let Some(error) = report.error.as_deref() {
        eprintln!("gbonds: warning: existing library could not be read: {error}");
    }
    let library = Arc::new(RwLock::new(library));
    let mut worker = WorkerHandle::spawn(paths.clone(), Arc::clone(&library));
    worker.send(WorkerCommand::SetWatch {
        enabled: false,
        roots: Vec::new(),
    });
    worker.send(WorkerCommand::Scan {
        roots: folders.folders.clone(),
        prune: true,
        force_analysis,
    });

    println!(
        "Scanning {} folder(s){}…",
        folders.folders.len(),
        if force_analysis {
            " with forced analysis"
        } else {
            ""
        }
    );
    let started = Instant::now();
    let mut last_stage = String::new();
    let mut last_percent = -1i32;
    let mut error: Option<String> = None;
    let mut finished = false;

    while !finished {
        std::thread::sleep(Duration::from_millis(80));
        for message in worker.drain() {
            match message {
                WorkerMessage::Started { message, .. } => {
                    eprintln!("{message}");
                }
                WorkerMessage::WatchTriggered(_) => {}
                WorkerMessage::Progress(progress) => {
                    let percent = if progress.total > 0 {
                        (progress.processed * 100 / progress.total) as i32
                    } else {
                        progress.percent.round() as i32
                    };
                    if progress.stage != last_stage || percent >= last_percent + 2 {
                        last_stage = progress.stage.clone();
                        last_percent = percent;
                        println!(
                            "{:<10} {:>3}%  {:>5}/{:<5} {}",
                            progress.stage,
                            percent.clamp(0, 100),
                            progress.processed,
                            progress.total,
                            progress.message
                        );
                    }
                }
                WorkerMessage::Finished(outcome) => {
                    finished = true;
                    error = outcome.error.clone();
                    if let Some(error) = outcome.error.as_deref() {
                        eprintln!("gbonds: scan failed: {error}");
                    } else {
                        println!("{}", outcome.summary);
                        println!(
                            "Done in {:.0}s: {} tracks, {} links, {} new, {} updated, {} analyzed, {} failed{}",
                            started.elapsed().as_secs_f64(),
                            outcome.tracks,
                            outcome.edges,
                            outcome.loaded,
                            outcome.updated,
                            outcome.analyzed,
                            outcome.failed,
                            if outcome.removed_ids.is_empty() {
                                String::new()
                            } else {
                                format!(", {} removed", outcome.removed_ids.len())
                            }
                        );
                    }
                }
                WorkerMessage::Error(message) => {
                    finished = true;
                    eprintln!("gbonds: {message}");
                    error = Some(message);
                }
            }
        }
    }
    worker.shutdown();
    if error.is_some() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn command_exists(program: &str) -> bool {
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {program} >/dev/null 2>&1"))
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// TUI
// ---------------------------------------------------------------------------

fn run_tui(paths: AppPaths, theme: ThemeMode) -> anyhow::Result<()> {
    let mut terminal = ratatui::try_init()?;
    let mut app = App::new(paths, theme);
    if let Err(error) = execute!(stdout(), EnableBracketedPaste, EnableMouseCapture) {
        app.shutdown();
        ratatui::restore();
        return Err(error.into());
    }

    let result = (|| -> anyhow::Result<()> {
        loop {
            if event::poll(Duration::from_millis(80))? {
                // Drain a burst of input (fast wheel scrolling, key repeats)
                // before redrawing. This keeps scrolling responsive instead of
                // redrawing once per queued event.
                for _ in 0..32 {
                    match event::read()? {
                        Event::Key(key) => app.handle_key(key),
                        Event::Mouse(mouse) => app.handle_mouse(mouse),
                        Event::Paste(text) => app.handle_paste(&text),
                        Event::Resize(_, _) => {}
                        _ => {}
                    }
                    if !event::poll(Duration::ZERO)? {
                        break;
                    }
                }
            }
            app.tick();
            terminal.draw(|frame| ui::draw(frame, &mut app))?;
            if app.should_quit {
                break;
            }
        }
        Ok(())
    })();

    app.shutdown();
    let _ = execute!(io::stdout(), DisableBracketedPaste, DisableMouseCapture);
    ratatui::restore();
    result
}
