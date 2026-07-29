//! Colour, resolved once.
//!
//! Drawing code names a *role* — `theme.dim`, `theme.selected()` — never a
//! colour, so the palette is one file rather than a scatter of literals.
//!
//! Where the colours come from: Herdr's own theme tokens, as `[theme.custom]` in
//! `config.toml` names them. The socket has no theme accessor, and the built-in
//! palettes are compiled into Herdr as numbers rather than strings, so a plugin
//! cannot read the theme it is sitting inside. What it can read is that file, and
//! the terminal's own ANSI slots underneath it — which is what `name =
//! "terminal"` means anyway.
//!
//! ▲ This is deliberately not shared with the explorer's `theme.rs`, which
//! resolves a syntect palette for previewing files and pulls two grammar crates
//! in with it. A popup that lists stashes has no files to highlight, and putting
//! ratatui plus syntect into `herdr-sdk` to share sixteen colour names would
//! make every plugin in the repo — including a Claude hook that never draws —
//! build a TUI stack. The shared part is the vocabulary, and that lives in
//! Herdr's documentation.

use std::path::PathBuf;
use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;

/// Herdr's theme tokens, named exactly as `config.toml` names them.
///
/// The defaults are ANSI slots rather than hexes: an indexed colour is whatever
/// the terminal says it is, which is the only way to match a palette that cannot
/// be read.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
struct Tokens {
    accent: Paint,
    overlay0: Paint,
    overlay1: Paint,
    text: Paint,
    subtext0: Paint,
    green: Paint,
    yellow: Paint,
    red: Paint,
    blue: Paint,
    mauve: Paint,
}

impl Default for Tokens {
    fn default() -> Self {
        Self {
            accent: Paint(Color::Cyan),
            overlay0: Paint(Color::DarkGray),
            overlay1: Paint(Color::Gray),
            text: Paint(Color::Reset),
            subtext0: Paint(Color::DarkGray),
            green: Paint(Color::Green),
            yellow: Paint(Color::Yellow),
            red: Paint(Color::Red),
            blue: Paint(Color::Blue),
            mauve: Paint(Color::Magenta),
        }
    }
}

/// One themed colour, as written in `config.toml`.
///
/// A wrapper only because ratatui's `Color` is not `Deserialize` — and the
/// parser it does have already takes `#rrggbb`, every ANSI name, and `reset`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Paint(Color);

impl<'de> Deserialize<'de> for Paint {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Color::from_str(&raw)
            .map(Paint)
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Debug, Clone)]
pub struct Theme {
    /// A stash's name, a pane's directory: ordinary content.
    pub text: Color,
    /// Secondary: ages, counts, paths.
    pub dim: Color,
    pub border: Color,
    /// Titles, the cursor, active affordances.
    pub accent: Color,
    /// An agent that would be resumed.
    pub agent: Color,
    /// A pane another plugin owns.
    pub plugin: Color,
    /// Something restored differently than recorded.
    pub warn: Color,
    /// Deleting a stash.
    pub danger: Color,
    /// A finished restore.
    pub ok: Color,
}

impl Theme {
    pub fn load() -> Self {
        let tokens = Config::load().theme.custom;
        Self {
            text: tokens.text.0,
            dim: tokens.subtext0.0,
            border: tokens.overlay0.0,
            accent: tokens.accent.0,
            agent: tokens.blue.0,
            plugin: tokens.mauve.0,
            warn: tokens.yellow.0,
            danger: tokens.red.0,
            ok: tokens.green.0,
        }
    }

    pub fn plain(&self) -> Style {
        Style::default().fg(self.text)
    }

    pub fn dimmed(&self) -> Style {
        Style::default().fg(self.dim)
    }

    pub fn title(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// The selected row.
    ///
    /// `REVERSED` over the accent rather than an explicit foreground and
    /// background: the terminal's own background becomes the foreground, so the
    /// row is legible under a light theme and a dark one without this file
    /// knowing which it is in.
    pub fn selected(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::REVERSED)
    }

    /// A clickable affordance, when it is the one that matters.
    pub fn button(&self, primary: bool) -> Style {
        match primary {
            true => Style::default()
                .fg(self.accent)
                .add_modifier(Modifier::REVERSED),
            false => Style::default().fg(self.dim),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Config {
    theme: ThemeSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ThemeSection {
    custom: Tokens,
}

impl Config {
    /// Any failure yields the ANSI base: a popup that refuses to draw over a
    /// malformed config line is worse than one drawn in the terminal's colours.
    fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&rewrite_rgb(&text)).unwrap_or_default()
    }
}

/// `$HERDR_CONFIG_PATH` when Herdr set it — it does for plugin processes — and
/// the documented default otherwise, so the popup is still themed when run by
/// hand.
fn config_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("HERDR_CONFIG_PATH") {
        return Some(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("herdr")
            .join("config.toml"),
    )
}

/// Turn `rgb(12, 34, 56)` into `#0c2238`.
///
/// Herdr documents four colour forms: hex, a name, `rgb(r,g,b)`, and `"reset"`.
/// Three of them are what ratatui's parser already takes; this covers the fourth.
fn rewrite_rgb(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("rgb(") {
        let (before, from) = rest.split_at(start);
        out.push_str(before);
        let Some(end) = from.find(')') else {
            break;
        };
        let parts: Vec<Option<u8>> = from[4..end]
            .split(',')
            .map(|part| part.trim().parse().ok())
            .collect();
        match parts.as_slice() {
            [Some(r), Some(g), Some(b)] => out.push_str(&format!("#{r:02x}{g:02x}{b:02x}")),
            _ => out.push_str(&from[..=end]),
        }
        rest = &from[end + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_tokens_override_the_ansi_base() {
        let config: Config = toml::from_str(
            r##"
            onboarding = false
            [ui]
            sidebar_width = 30
            [theme]
            name = "terminal"
            [theme.custom]
            accent = "#f5c2e7"
            red = "magenta"
            "##,
        )
        .expect("parsing a realistic config");
        assert_eq!(
            config.theme.custom.accent,
            Paint(Color::Rgb(0xf5, 0xc2, 0xe7))
        );
        assert_eq!(config.theme.custom.red, Paint(Color::Magenta));
        // Untouched tokens keep the base rather than going black.
        assert_eq!(config.theme.custom.text, Paint(Color::Reset));
    }

    #[test]
    fn herdrs_rgb_form_is_understood() {
        let config: Config = toml::from_str(&rewrite_rgb(
            r#"
            [theme.custom]
            accent = "rgb(245, 194, 231)"
            "#,
        ))
        .expect("parsing an rgb() colour");
        assert_eq!(config.theme.custom.accent, Paint(Color::Rgb(245, 194, 231)));
    }

    #[test]
    fn a_malformed_colour_falls_back_rather_than_refusing_to_draw() {
        assert!(
            toml::from_str::<Config>(
                r#"
                [theme.custom]
                accent = "not a colour"
                "#,
            )
            .is_err()
        );
        // Which is why `load` swallows it.
        assert_eq!(Config::default().theme.custom.accent, Paint(Color::Cyan));
    }
}
