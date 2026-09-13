//! User configuration for the terminal window.
//!
//! Read from `--config <path>`, `$FORGE_CONFIG`, or
//! `$XDG_CONFIG_HOME/forge/config.toml` (`~/.config/forge/config.toml`).
//! Every key is optional; a missing file yields the defaults below.

use proto_ipc::Rgb;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    pub colors: ColorConfig,
    pub terminal: TerminalConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Font family passed to the platform text system; `monospace` resolves
    /// through fontconfig on Linux.
    pub family: String,
    /// Font size in pixels.
    pub size: f32,
    /// Cell height as a multiple of the font size.
    pub line_height: f32,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ColorConfig {
    pub background: HexColor,
    pub foreground: HexColor,
    pub cursor: HexColor,
    pub selection: HexColor,
    /// Opacity of the selection overlay, 0–1.
    pub selection_opacity: f32,
    /// Status line text.
    pub accent: HexColor,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// Program to run; defaults to `$SHELL`, then `/bin/sh`.
    pub shell: Option<String>,
    pub args: Vec<String>,
    /// Space between the window edge and the grid, in pixels.
    pub padding: f32,
    /// Show the "Forge · cols×rows · status" line above the grid.
    pub show_status: bool,
}

/// `#rrggbb` colour that deserializes from a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexColor(pub Rgb);

impl HexColor {
    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self(Rgb { r, g, b })
    }

    /// Parses `#rrggbb` or `rrggbb`.
    ///
    /// # Errors
    ///
    /// Returns the offending text when it is not six hexadecimal digits.
    pub fn parse(text: &str) -> Result<Self, String> {
        let digits = text.trim().trim_start_matches('#');
        let value = match digits.len() {
            6 => u32::from_str_radix(digits, 16).ok(),
            _ => None,
        }
        .ok_or_else(|| format!("expected #rrggbb, got {text:?}"))?;
        #[allow(clippy::cast_possible_truncation)]
        Ok(Self::new(
            (value >> 16) as u8,
            (value >> 8 & 0xff) as u8,
            (value & 0xff) as u8,
        ))
    }
}

impl<'de> Deserialize<'de> for HexColor {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: "monospace".into(),
            size: 14.0,
            line_height: 1.3,
        }
    }
}

impl Default for ColorConfig {
    fn default() -> Self {
        Self {
            background: HexColor::new(0x11, 0x13, 0x18),
            foreground: HexColor::new(0xd8, 0xde, 0xe9),
            cursor: HexColor::new(0xeb, 0xcb, 0x8b),
            selection: HexColor::new(0x88, 0xc0, 0xd0),
            selection_opacity: 0.35,
            accent: HexColor::new(0x88, 0xc0, 0xd0),
        }
    }
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            shell: None,
            args: Vec::new(),
            padding: 16.0,
            show_status: true,
        }
    }
}

impl Config {
    /// Parses a TOML document.
    ///
    /// # Errors
    ///
    /// Reports unknown keys and malformed values with their location.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text)
            .map(Self::sanitized)
            .map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })
    }

    /// Loads `path`; a missing file is not an error and yields the defaults.
    ///
    /// # Errors
    ///
    /// Returns read failures other than "not found", and parse errors.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text, path),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.to_path_buf(),
                source,
            }),
        }
    }

    /// Resolves the config path from an explicit argument, `$FORGE_CONFIG`,
    /// or the XDG config directory.
    #[must_use]
    pub fn default_path(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return Some(path.to_path_buf());
        }
        if let Some(path) = std::env::var_os("FORGE_CONFIG") {
            return Some(PathBuf::from(path));
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(base.join("forge").join("config.toml"))
    }

    /// The shell to spawn: configured, else `$SHELL`, else `/bin/sh`.
    #[must_use]
    pub fn shell(&self) -> String {
        self.terminal
            .shell
            .clone()
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".into())
    }

    /// Clamps values that would make the window unusable.
    fn sanitized(mut self) -> Self {
        self.font.size = self.font.size.clamp(4.0, 200.0);
        self.font.line_height = self.font.line_height.clamp(0.8, 4.0);
        self.terminal.padding = self.terminal.padding.clamp(0.0, 200.0);
        self.colors.selection_opacity = self.colors.selection_opacity.clamp(0.0, 1.0);
        self
    }
}

/// Monospace families tried, in order, when the configured family is a
/// generic name (`monospace`) or is not installed.
pub const MONOSPACE_CANDIDATES: &[&str] = &[
    "DejaVu Sans Mono",
    "Ubuntu Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "Menlo",
    "Consolas",
    "Cascadia Mono",
    "Courier New",
    "JetBrains Mono",
    "Fira Code",
    "Hack",
    "Source Code Pro",
];

/// Picks the family the text system should load. GPUI matches family names
/// literally, so `monospace` and other generic aliases would silently fall
/// back to a proportional UI font; this maps them to an installed monospace
/// family instead. Returns the requested name untouched when nothing better
/// is installed.
#[must_use]
pub fn resolve_font_family(requested: &str, installed: &[String]) -> String {
    let find = |name: &str| {
        installed
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(name))
            .cloned()
    };
    let generic = matches!(
        requested.trim().to_ascii_lowercase().as_str(),
        "monospace" | "mono" | "fixed" | ""
    );
    if !generic && let Some(exact) = find(requested) {
        return exact;
    }
    MONOSPACE_CANDIDATES
        .iter()
        .find_map(|name| find(name))
        .unwrap_or_else(|| requested.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colors_with_or_without_hash() {
        assert_eq!(
            HexColor::parse("#88c0d0").unwrap(),
            HexColor::new(0x88, 0xc0, 0xd0)
        );
        assert_eq!(
            HexColor::parse("  FF0000 ").unwrap(),
            HexColor::new(0xff, 0, 0)
        );
        assert!(HexColor::parse("#fff").is_err());
        assert!(HexColor::parse("#gg0000").is_err());
    }

    #[test]
    fn empty_document_yields_defaults_and_unknown_keys_are_rejected() {
        let path = Path::new("config.toml");
        assert_eq!(Config::from_toml("", path).unwrap(), Config::default());
        let error = Config::from_toml("[font]\nfamly = \"x\"\n", path).unwrap_err();
        assert!(error.to_string().contains("famly"), "{error}");
    }

    #[test]
    fn parses_partial_overrides_and_clamps_extremes() {
        let text = r##"
            [font]
            size = 1000
            [colors]
            cursor = "#ff0000"
            selection_opacity = 3
            [terminal]
            shell = "/bin/bash"
            args = ["-l"]
            show_status = false
        "##;
        let config = Config::from_toml(text, Path::new("config.toml")).unwrap();
        assert_eq!(config.font.family, "monospace");
        assert!((config.font.size - 200.0).abs() < f32::EPSILON);
        assert_eq!(config.colors.cursor, HexColor::new(0xff, 0, 0));
        assert!((config.colors.selection_opacity - 1.0).abs() < f32::EPSILON);
        assert_eq!(config.shell(), "/bin/bash");
        assert_eq!(config.terminal.args, vec!["-l".to_string()]);
        assert!(!config.terminal.show_status);
    }

    #[test]
    fn generic_and_missing_families_map_to_an_installed_monospace_font() {
        let installed = ["Ubuntu", "Ubuntu Mono", "dejavu sans mono", "Fira Code"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            resolve_font_family("monospace", &installed),
            "dejavu sans mono"
        );
        assert_eq!(resolve_font_family("fira code", &installed), "Fira Code");
        assert_eq!(
            resolve_font_family("Comic Mono", &installed),
            "dejavu sans mono"
        );
        assert_eq!(resolve_font_family("Comic Mono", &[]), "Comic Mono");
    }

    #[test]
    fn missing_file_is_the_default_config() {
        let missing = std::env::temp_dir().join("forge-config-does-not-exist.toml");
        assert_eq!(Config::load(&missing).unwrap(), Config::default());
    }

    #[test]
    fn explicit_path_wins_over_environment() {
        let explicit = Path::new("/tmp/explicit.toml");
        assert_eq!(
            Config::default_path(Some(explicit)),
            Some(explicit.to_path_buf())
        );
        assert!(
            Config::default_path(None).is_none_or(|path| path.ends_with("forge/config.toml")
                || std::env::var_os("FORGE_CONFIG").is_some())
        );
    }
}
