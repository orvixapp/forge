//! User configuration for the shell and terminal.
//!
//! Format: TOML **[DECIDIDO en Fase 1]**. Layers, in ascending precedence:
//! built-in defaults → user file (`--config <path>`, `$FORGE_CONFIG`, or
//! `$XDG_CONFIG_HOME/forge/config.toml`) → workspace file
//! (`<cwd>/.forge/config.toml`). Every key is optional. A JSON Schema for
//! editors is generated from these types (`forge-gui --print-config-schema`,
//! committed as `docs/config.schema.json`).

use crate::shell::UserKeyBinding;
use proto_ipc::Rgb;
use schemars::JsonSchema;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;
use toml::{Table, Value};

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

/// Keys the workspace layer may not set: a repository must not be able to
/// pick the program Forge executes on the user's machine.
pub const WORKSPACE_FORBIDDEN_KEYS: &[&[&str]] = &[&["terminal", "shell"], &["terminal", "args"]];

#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    /// Overrides applied on top of the selected theme.
    pub colors: ColorOverrides,
    pub terminal: TerminalConfig,
    pub ui: UiConfig,
    pub keybindings: Vec<UserKeyBinding>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Spanish,
    English,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct UiConfig {
    pub language: Language,
    /// Built-in theme (`forge-dark`, `forge-light`) or the stem of a file in
    /// `themes/` next to the config.
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: Language::Spanish,
            theme: crate::theme::FORGE_DARK.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FontConfig {
    /// Font family; generic names such as `monospace` resolve to an installed
    /// monospace family.
    pub family: String,
    /// Font size in pixels.
    pub size: f32,
    /// Cell height as a multiple of the font size.
    pub line_height: f32,
}

/// Per-key colour overrides; unset keys come from the theme.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ColorOverrides {
    pub background: Option<HexColor>,
    pub foreground: Option<HexColor>,
    pub cursor: Option<HexColor>,
    pub selection: Option<HexColor>,
    /// Opacity of the selection overlay, 0–1.
    pub selection_opacity: Option<f32>,
    /// Status line text.
    pub accent: Option<HexColor>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// Program to run; defaults to `$SHELL`, then `/bin/sh`. Ignored in the
    /// workspace layer.
    pub shell: Option<String>,
    /// Ignored in the workspace layer.
    pub args: Vec<String>,
    /// Space between the pane edge and the grid, in pixels.
    pub padding: f32,
    /// Show the "Forge · cols×rows · status" line above the grid.
    pub show_status: bool,
}

/// `#rrggbb` colour that deserializes from a string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HexColor(pub Rgb);

impl HexColor {
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
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

impl JsonSchema for HexColor {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "HexColor".into()
    }

    fn json_schema(_generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "Colour as #rrggbb",
            "pattern": "^#?[0-9a-fA-F]{6}$"
        })
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

/// Which files contributed to a loaded configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigSources {
    pub user: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
}

impl Config {
    /// Parses a TOML document.
    ///
    /// # Errors
    ///
    /// Reports unknown keys and malformed values with their location.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let table = parse_table(text, path)?;
        Self::from_table(table, path)
    }

    fn from_table(table: Table, path: &Path) -> Result<Self, ConfigError> {
        Value::Table(table)
            .try_into()
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
        Self::load_layers(Some(path), None).map(|(config, _)| config)
    }

    /// Loads the user file and, on top of it, `<workspace>/.forge/config.toml`
    /// with the keys in [`WORKSPACE_FORBIDDEN_KEYS`] removed. Missing files
    /// are skipped; a present but invalid file is an error so the caller can
    /// keep the configuration that was already running.
    ///
    /// # Errors
    ///
    /// Read failures other than "not found", and parse errors, naming the file.
    pub fn load_layers(
        user: Option<&Path>,
        workspace: Option<&Path>,
    ) -> Result<(Self, ConfigSources), ConfigError> {
        let mut merged = Table::new();
        let mut sources = ConfigSources::default();
        if let Some(path) = user
            && let Some(table) = read_table(path)?
        {
            merge_tables(&mut merged, table);
            sources.user = Some(path.to_path_buf());
        }
        if let Some(dir) = workspace {
            let path = dir.join(".forge").join("config.toml");
            if let Some(mut table) = read_table(&path)? {
                for key_path in WORKSPACE_FORBIDDEN_KEYS {
                    remove_key(&mut table, key_path);
                }
                merge_tables(&mut merged, table);
                sources.workspace = Some(path);
            }
        }
        let anchor = sources
            .workspace
            .clone()
            .or_else(|| sources.user.clone())
            .unwrap_or_else(|| PathBuf::from("config.toml"));
        Ok((Self::from_table(merged, &anchor)?, sources))
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

    /// JSON Schema describing `config.toml`, for editor validation.
    ///
    /// # Panics
    ///
    /// Never in practice: the schema is a plain JSON value.
    #[must_use]
    pub fn json_schema() -> String {
        let schema = schemars::schema_for!(Config);
        serde_json::to_string_pretty(&schema).expect("schema is serializable")
    }

    /// Clamps values that would make the window unusable.
    fn sanitized(mut self) -> Self {
        self.font.size = self.font.size.clamp(4.0, 200.0);
        self.font.line_height = self.font.line_height.clamp(0.8, 4.0);
        self.terminal.padding = self.terminal.padding.clamp(0.0, 200.0);
        self.colors.selection_opacity = self
            .colors
            .selection_opacity
            .map(|opacity| opacity.clamp(0.0, 1.0));
        self
    }
}

fn parse_table(text: &str, path: &Path) -> Result<Table, ConfigError> {
    text.parse::<Table>().map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

fn read_table(path: &Path) -> Result<Option<Table>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_table(&text, path).map(Some),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Deep merge: tables merge key by key, everything else is replaced.
fn merge_tables(base: &mut Table, layer: Table) {
    for (key, value) in layer {
        match (base.get_mut(&key), value) {
            (Some(Value::Table(existing)), Value::Table(incoming)) => {
                merge_tables(existing, incoming);
            }
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

fn remove_key(table: &mut Table, path: &[&str]) {
    match path {
        [] => {}
        [key] => {
            table.remove(*key);
        }
        [key, rest @ ..] => {
            if let Some(Value::Table(child)) = table.get_mut(*key) {
                remove_key(child, rest);
            }
        }
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
            [ui]
            language = "english"
            theme = "forge-light"
            [[keybindings]]
            command = "terminal.newTab"
            keys = "ctrl+n"
        "##;
        let config = Config::from_toml(text, Path::new("config.toml")).unwrap();
        assert_eq!(config.font.family, "monospace");
        assert!((config.font.size - 200.0).abs() < f32::EPSILON);
        assert_eq!(config.colors.cursor, Some(HexColor::new(0xff, 0, 0)));
        assert_eq!(config.colors.background, None);
        assert_eq!(config.colors.selection_opacity, Some(1.0));
        assert_eq!(config.shell(), "/bin/bash");
        assert_eq!(config.terminal.args, vec!["-l".to_string()]);
        assert!(!config.terminal.show_status);
        assert_eq!(config.ui.language, Language::English);
        assert_eq!(config.ui.theme, "forge-light");
        assert_eq!(config.keybindings.len(), 1);
    }

    #[test]
    fn workspace_layer_merges_deeply_but_cannot_change_the_shell() {
        let root = std::env::temp_dir().join(format!("forge-config-layers-{}", std::process::id()));
        let workspace = root.join("repo");
        std::fs::create_dir_all(workspace.join(".forge")).unwrap();
        let user = root.join("config.toml");
        std::fs::write(
            &user,
            "[font]\nsize = 12\n[terminal]\nshell = \"/bin/zsh\"\npadding = 4\n",
        )
        .unwrap();
        std::fs::write(
            workspace.join(".forge/config.toml"),
            "[font]\nfamily = \"Hack\"\n[terminal]\nshell = \"/tmp/evil\"\nargs = [\"-c\", \"rm\"]\npadding = 2\n",
        )
        .unwrap();
        let (config, sources) = Config::load_layers(Some(&user), Some(&workspace)).unwrap();
        assert_eq!(config.font.family, "Hack");
        assert!((config.font.size - 12.0).abs() < f32::EPSILON);
        assert_eq!(config.shell(), "/bin/zsh");
        assert!(config.terminal.args.is_empty());
        assert!((config.terminal.padding - 2.0).abs() < f32::EPSILON);
        assert_eq!(sources.user.as_deref(), Some(user.as_path()));
        assert!(sources.workspace.is_some());

        std::fs::write(workspace.join(".forge/config.toml"), "not toml = [").unwrap();
        let error = Config::load_layers(Some(&user), Some(&workspace)).unwrap_err();
        assert!(error.to_string().contains(".forge"), "{error}");
        std::fs::remove_dir_all(root).unwrap();
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
        assert!(Config::default_path(None).is_none_or(|path| path.ends_with("forge/config.toml")
            || std::env::var_os("FORGE_CONFIG").is_some()));
    }

    #[test]
    fn json_schema_documents_every_section() {
        let schema = Config::json_schema();
        for key in ["font", "colors", "terminal", "ui", "keybindings", "HexColor"] {
            assert!(schema.contains(key), "schema lacks {key}");
        }
    }
}
