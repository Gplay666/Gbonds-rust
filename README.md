# Gbonds - Rust

Terminal music player for a local music library. It scans folders, analyzes
audio, builds a similarity graph, and plays tracks through mpv.

## Install

Requirements:

- Rust 1.85 or newer
- ffmpeg and ffprobe in PATH
- mpv in PATH, with JSON IPC support

Local install:

```sh
make install
```

This builds the release binary and copies it to `~/.local/bin/gbonds`. That
directory is normally in `PATH`. If `gbonds` is not found after installing,
add it to your shell profile:

```sh
export PATH="$HOME/.local/bin:$PATH"
```

To install somewhere else:

```sh
sudo make install PREFIX=/usr/local
```

Other make targets: `build`, `test`, `fmt`, `fmt-check`, `check`, `uninstall`,
`clean`.

You can also install with Cargo:

```sh
cargo install --path . --locked
```

Cargo puts the binary in `~/.cargo/bin`.

Linux is tested. macOS should work. Windows is not tested because mpv IPC and
XDG paths differ.

## First run

```sh
gbonds
```

Press `o` to add a music folder. gbonds reads tags and filenames, builds the
similarity graph, and analyzes tracks in the background. Press `?` to see all
keys. The theme follows the terminal background and can be changed with `t` or
`GBONDS_THEME`.

## Data

gbonds stores its cache in `~/.local/share/gbonds` (or
`$XDG_DATA_HOME/gbonds`):

```
library.json          format: tui-music-library, version 1
folders.json          format: tui-music-folders, version 1
playlists.json        format: tui-music-playlists, version 1
analysis_state.json   format: tui-music-analysis-state, version 1
analysis.log
```

On the first launch with the default data directory, missing files are copied
from `~/.local/share/tui-music-recomendator`. The Python cache is not changed.
Later scans write only to the gbonds directory.

Delete `~/.local/share/gbonds` to start with an empty library. The JSON formats
and field names match the Python application, and unknown fields are
preserved.

Environment variables:

```
GBONDS_DATA_DIR        cache directory (default: $XDG_DATA_HOME/gbonds)
GBONDS_LIBRARY         library.json path
GBONDS_FOLDERS         folders.json path
GBONDS_PLAYLISTS       playlists.json path
GBONDS_ANALYSIS_STATE  analysis_state.json path
GBONDS_LOG             analysis.log path
GBONDS_THEME           auto, dark, or light
GBONDS_BACKGROUND      force dark or light detection
GBONDS_MPV_AO          mpv audio output (null for silence)
GBONDS_BACKEND         null disables playback
GBONDS_WATCH_INTERVAL  watcher poll interval in seconds
```

`TUI_MUSIC_*`, `TUI_BACKGROUND`, and `TUI_LIGHT` are still accepted.

## Command line

```
gbonds                    open the TUI
gbonds --status           show library and analysis status
gbonds --folders          show folders and playlists
gbonds --scan             import new or changed files, then exit
gbonds --analyze          re-analyze all tracks, then exit
gbonds --add-folder PATH  add a folder
gbonds --data-dir DIR     use another data directory
gbonds --library FILE     use another library.json
gbonds --theme MODE       auto, dark, or light
```

Run `gbonds --help` for all options.

## Keys

```
j/k, up/down      move selection
Ctrl-d/Ctrl-u     page down/up
gg/G              top/bottom
h/l, Tab          change pane
1-4               switch view
f                 jump to playing track
Enter             play selection
Space             pause
/                 search, n/N next/previous match
r, R              similar tracks, radio
a, A              add selection, visible list to a playlist
e, E              queue selection, visible list
s, S              shuffle, repeat
+/-               volume
M                 mute
:                 command line
?                 help
q, ZZ             quit
```

The mouse wheel scrolls the list. Clicking a track selects it and
double-clicking plays it. Arrow keys move the selection and scroll the list
only when the selection reaches an edge.

Full key map: `?` in the app.

## Commands

Type `:` to open the command line. `Tab` completes commands.

```
:scan, :rescan          import new or changed files
:analyze, :analyze!     analyze missing tracks, re-analyze all
:theme [MODE]           cycle or set auto/dark/light
:sort FIELD             sort by id, title, artist, album, year, bpm, rating, duration
:filter TEXT, :clear    set or clear the library filter
:recs, :radio [N]       similar tracks, radio queue
:play [ID], :enqueue    play or queue a track
:playlist ...           add, play, rename, clear
:watch on|off|toggle    folder watcher
:w, :wq, :q             save, save and quit, quit
:reload                 reload the cache from disk
:help, :version         information
```

## How recommendations work

Each track is described by audio features: timbre/MFCC, chroma, tempo, key,
energy, spectral shape, and loudness. The analyzer links each track to its
closest neighbors and to tracks by the same artist or album.

For You follows this graph within two steps. `r` finds the nearest tracks in
feature space. `R` builds a randomized queue from the similarity pool. No
network requests are made.

## License

AGPL-3.0-or-later. See [LICENSE](LICENSE).
