//! Terminal theme.
//!
//! Gbonds deliberately uses the terminal's ANSI palette instead of fixed RGB
//! values, so the UI follows the colors configured by the user's terminal.
//! Light and dark modes only choose which named accents have enough contrast.

use std::env;

use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThemeMode {
    Auto,
    Dark,
    Light,
}

impl ThemeMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_lowercase().as_str() {
            "auto" | "system" | "default" => Some(Self::Auto),
            "dark" | "night" => Some(Self::Dark),
            "light" | "day" => Some(Self::Light),
            _ => None,
        }
    }

    pub fn from_env() -> Self {
        env::var("GBONDS_THEME")
            .or_else(|_| env::var("TUI_THEME"))
            .ok()
            .and_then(|value| Self::parse(&value))
            .unwrap_or(Self::Auto)
    }

    pub fn cycle(self) -> Self {
        match self {
            Self::Auto => Self::Dark,
            Self::Dark => Self::Light,
            Self::Light => Self::Auto,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Dark => "dark",
            Self::Light => "light",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub resolved: &'static str,
    pub accent: Color,
    pub text: Color,
    pub dim: Color,
    pub border: Color,
    pub good: Color,
    pub warn: Color,
    pub error: Color,
    pub selection: Style,
    pub selection_inactive: Style,
}

impl Theme {
    pub fn for_mode(mode: ThemeMode) -> Self {
        let resolved_dark = match mode {
            ThemeMode::Dark => true,
            ThemeMode::Light => false,
            ThemeMode::Auto => detect_dark_background(),
        };
        let accent = if resolved_dark {
            Color::Cyan
        } else {
            Color::Blue
        };
        Self {
            resolved: if resolved_dark { "dark" } else { "light" },
            accent,
            text: Color::Reset,
            dim: Color::DarkGray,
            border: Color::DarkGray,
            good: Color::Green,
            warn: Color::Yellow,
            error: Color::Red,
            selection: Style::new()
                .fg(Color::Reset)
                .add_modifier(Modifier::REVERSED | Modifier::BOLD),
            selection_inactive: Style::new().fg(accent),
        }
    }

    pub fn title(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn strong(&self) -> Style {
        Style::new().fg(self.text).add_modifier(Modifier::BOLD)
    }

    pub fn muted(&self) -> Style {
        Style::new().fg(self.dim)
    }

    pub fn warn(&self) -> Style {
        Style::new().fg(self.warn)
    }

    pub fn error(&self) -> Style {
        Style::new().fg(self.error)
    }
}

/// Reads common terminal hints. `COLORFGBG` is set by several terminals and
/// has the form `foreground;background`; ANSI backgrounds 0-7 are dark.
fn detect_dark_background() -> bool {
    if let Ok(value) = env::var("GBONDS_BACKGROUND").or_else(|_| env::var("TUI_BACKGROUND")) {
        match value.trim().to_lowercase().as_str() {
            "light" => return false,
            "dark" => return true,
            _ => {}
        }
    }
    if env::var("GBONDS_LIGHT").is_ok() || env::var("TUI_LIGHT").is_ok() {
        return false;
    }
    let Some(value) = env::var("COLORFGBG").ok() else {
        return true;
    };
    let Some(background) = value.rsplit(';').next() else {
        return true;
    };
    match background.trim().parse::<u16>() {
        Ok(index) => index < 8,
        Err(_) => true,
    }
}
