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
pub const WORKSPACE_FORBIDDEN_KEYS: &[&[&str]] = &[
    &["terminal", "shell"],
    &["terminal", "args"],
    &["profiles"],
    &["agents"],
    &["providers"],
    &["router"],
    &["mcp_servers"],
    &["languages"],
];

#[derive(Debug, Clone, Default, PartialEq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub font: FontConfig,
    /// Overrides applied on top of the selected theme.
    pub colors: ColorOverrides,
    pub terminal: TerminalConfig,
    pub ui: UiConfig,
    pub editor: EditorConfig,
    pub keybindings: Vec<UserKeyBinding>,
    /// Named terminal setups for `terminal.newTabWithProfile`. Ignored in
    /// the workspace layer, like `terminal.shell`.
    pub profiles: Vec<TerminalProfile>,
    /// ACP adapters available to `agent.newSession`. Ignored in workspace
    /// configuration because repositories must not choose executables.
    pub agents: Vec<AgentConfig>,
    /// Model providers Forge injects into agents (§17.7). User layer only.
    pub providers: Vec<ProviderConfig>,
    /// Which provider serves each task class (§17.7).
    pub router: RouterConfig,
    /// MCP servers handed to agents in `session/new` (§18). User layer only:
    /// a repository must not make Forge start executables.
    pub mcp_servers: Vec<McpServerConfig>,
    pub lsp: LspConfig,
    /// User-only language/server overrides; never execute repo-supplied commands.
    pub languages: Vec<forge_lsp::LanguageDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LspConfig {
    pub enabled: bool,
    pub debounce_ms: u64,
    pub startup_ms: u64,
    pub idle_shutdown_secs: u64,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 75,
            startup_ms: 500,
            idle_shutdown_secs: 1800,
        }
    }
}

/// A model provider: how an agent should reach a model, expressed as the
/// environment the agent's CLI already understands. Keys are never written
/// in the config: `api_key_env` names the variable that holds one.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub name: String,
    pub kind: ProviderKind,
    /// Model id passed to the agent (`gpt-5`, `claude-opus-5`, …); empty
    /// keeps the agent's default.
    pub model: String,
    /// OpenAI-compatible or custom endpoint; empty keeps the default.
    pub base_url: String,
    /// Environment variable holding the API key; empty relies on the
    /// agent's own login (Codex with the `ChatGPT` subscription, Claude Code
    /// with `claude login`).
    pub api_key_env: String,
    /// Whether requests are free of charge (local models, free tiers);
    /// the router prefers these for `trivial` tasks.
    pub free: bool,
    /// Extra environment for this provider, verbatim.
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Openai,
    Anthropic,
    Google,
    /// Any server speaking the `OpenAI` API (Ollama, LM Studio, vLLM…).
    OpenaiCompatible,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: ProviderKind::Openai,
            model: String::new(),
            base_url: String::new(),
            api_key_env: String::new(),
            free: false,
            env: std::collections::BTreeMap::new(),
        }
    }
}

/// Provider name per task class; an empty entry means "the agent's own
/// default". `/trivial`, `/normal` or `/deep` at the start of a prompt
/// picks the class; `default_class` applies otherwise.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RouterConfig {
    pub trivial: String,
    pub normal: String,
    pub deep: String,
    pub default_class: TaskClass,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            trivial: String::new(),
            normal: String::new(),
            deep: String::new(),
            default_class: TaskClass::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum TaskClass {
    /// Renames, one-liners, questions: cheap/free models are enough.
    Trivial,
    #[default]
    Normal,
    /// Architecture, debugging across files: the strongest model.
    Deep,
}

impl TaskClass {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Trivial => "trivial",
            Self::Normal => "normal",
            Self::Deep => "deep",
        }
    }

    /// Splits a `/trivial|/normal|/deep` prefix off a prompt.
    #[must_use]
    pub fn split_prefix(prompt: &str) -> (Option<Self>, &str) {
        let trimmed = prompt.trim_start();
        for (prefix, class) in [
            ("/trivial", Self::Trivial),
            ("/normal", Self::Normal),
            ("/deep", Self::Deep),
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix)
                && (rest.is_empty() || rest.starts_with(char::is_whitespace))
            {
                return (Some(class), rest.trim_start());
            }
        }
        (None, prompt)
    }
}

impl RouterConfig {
    /// Provider name for a class, if configured.
    #[must_use]
    pub fn provider_for(&self, class: TaskClass) -> Option<&str> {
        let name = match class {
            TaskClass::Trivial => &self.trivial,
            TaskClass::Normal => &self.normal,
            TaskClass::Deep => &self.deep,
        };
        (!name.is_empty()).then_some(name.as_str())
    }
}

/// An MCP server the agent connects to directly (ACP `session/new`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    /// Streamable HTTP endpoint (HTTPS, or HTTP on loopback only).
    pub url: String,
    /// Header values expressed as `${ENV_VAR}` references, never tokens.
    pub headers: std::collections::BTreeMap<String, String>,
    pub enabled: bool,
    /// Empty means all agents; otherwise matches configured agent names.
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum McpTransport {
    #[default]
    Stdio,
    Http,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: McpTransport::Stdio,
            command: String::new(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            url: String::new(),
            headers: std::collections::BTreeMap::new(),
            enabled: true,
            agents: Vec::new(),
        }
    }
}

impl ProviderConfig {
    /// Environment that points the agent's CLI at this provider: the
    /// well-known variables of each ecosystem plus `FORGE_PROVIDER_*` for
    /// adapters that want to read the route explicitly.
    #[must_use]
    pub fn environment(&self) -> Vec<(String, String)> {
        let mut env = vec![
            ("FORGE_PROVIDER".to_owned(), self.name.clone()),
            (
                "FORGE_PROVIDER_KIND".to_owned(),
                match self.kind {
                    ProviderKind::Openai => "openai",
                    ProviderKind::Anthropic => "anthropic",
                    ProviderKind::Google => "google",
                    ProviderKind::OpenaiCompatible => "openai-compatible",
                }
                .to_owned(),
            ),
        ];
        let key = (!self.api_key_env.is_empty())
            .then(|| std::env::var(&self.api_key_env).ok())
            .flatten();
        let set = |env: &mut Vec<(String, String)>, name: &str, value: &str| {
            if !value.is_empty() {
                env.push((name.to_owned(), value.to_owned()));
            }
        };
        if !self.model.is_empty() {
            env.push(("FORGE_PROVIDER_MODEL".to_owned(), self.model.clone()));
        }
        match self.kind {
            ProviderKind::Openai | ProviderKind::OpenaiCompatible => {
                set(&mut env, "OPENAI_BASE_URL", &self.base_url);
                set(&mut env, "OPENAI_MODEL", &self.model);
                if let Some(key) = &key {
                    set(&mut env, "OPENAI_API_KEY", key);
                }
            }
            ProviderKind::Anthropic => {
                set(&mut env, "ANTHROPIC_BASE_URL", &self.base_url);
                set(&mut env, "ANTHROPIC_MODEL", &self.model);
                if let Some(key) = &key {
                    set(&mut env, "ANTHROPIC_API_KEY", key);
                    // Gateways (Token Harbor, LiteLLM…) take the key as a
                    // bearer token, which is what Claude Code sends from
                    // `ANTHROPIC_AUTH_TOKEN`; direct Anthropic ignores it.
                    if !self.base_url.is_empty() {
                        set(&mut env, "ANTHROPIC_AUTH_TOKEN", key);
                    }
                }
            }
            ProviderKind::Google => {
                set(&mut env, "GOOGLE_GEMINI_BASE_URL", &self.base_url);
                set(&mut env, "GEMINI_MODEL", &self.model);
                if let Some(key) = &key {
                    set(&mut env, "GEMINI_API_KEY", key);
                }
            }
        }
        env.extend(self.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env
    }

    /// Short description for the route line: `openai:gpt-5 (free)`.
    #[must_use]
    pub fn describe(&self) -> String {
        let kind = match self.kind {
            ProviderKind::Openai => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Google => "google",
            ProviderKind::OpenaiCompatible => "openai-compatible",
        };
        let model = if self.model.is_empty() {
            "modelo por defecto"
        } else {
            &self.model
        };
        format!(
            "{}: {kind}:{model}{}",
            self.name,
            if self.free { " (free)" } else { "" }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: std::collections::BTreeMap<String, String>,
    pub auth_method: Option<String>,
    pub enabled: bool,
    /// Provider used when the router has nothing for the task class;
    /// empty keeps the agent's own login/model.
    pub provider: String,
    /// Run each session in its own git worktree (`<config>/worktrees/…`)
    /// so agents never edit the checkout you are working in.
    pub worktree: bool,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: std::collections::BTreeMap::new(),
            auth_method: None,
            enabled: true,
            provider: String::new(),
            worktree: false,
        }
    }
}

// Configuration flags are a struct of booleans by nature.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EditorConfig {
    /// Columns per tab stop; also the width of a soft tab.
    pub tab_size: usize,
    /// Insert `\t` instead of spaces on Tab.
    pub indent_with_tabs: bool,
    pub line_numbers: bool,
    /// Save dirty files this long after the last edit; 0 disables.
    pub autosave_ms: u64,
    /// Wrap long lines at the pane width (`Alt+Z` toggles per editor).
    pub word_wrap: bool,
    /// Show the minimap strip at the right of editors.
    pub minimap: bool,
    /// Helix-like modal editing: files open in Normal mode (`i` inserts,
    /// `Esc` returns, `hjkl`/`w`/`b`/`0`/`$`/`G` move, `x`/`d` delete,
    /// `u`/`U` undo/redo, `v` extends, `y`/`p` copy/paste, `/` finds).
    pub modal: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            tab_size: 4,
            indent_with_tabs: false,
            line_numbers: true,
            autosave_ms: 0,
            word_wrap: false,
            minimap: true,
            modal: false,
        }
    }
}

/// A way to start a terminal: program, arguments, directory, environment.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TerminalProfile {
    pub name: String,
    /// Program to run; defaults to `terminal.shell`.
    #[serde(default)]
    pub shell: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Starting directory; defaults to the active tab's.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
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
    /// Inject Forge's shell integration (OSC 133 prompt marks, OSC 7 cwd)
    /// into bash, zsh and fish started without custom `args`.
    pub shell_integration: bool,
    /// What to do when a program writes the clipboard (OSC 52).
    pub clipboard_write: ClipboardPolicy,
    /// Command that opens `file:line` references from Ctrl+click; `{file}`,
    /// `{line}` and `{column}` are substituted. Empty uses the desktop
    /// opener (`xdg-open`/`open`).
    pub open_file_command: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClipboardPolicy {
    /// Show a confirmation the first time per tab.
    Ask,
    Allow,
    Deny,
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
            shell_integration: true,
            clipboard_write: ClipboardPolicy::Ask,
            open_file_command: Vec::new(),
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
        self.editor.tab_size = self.editor.tab_size.clamp(1, 16);
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
    fn task_classes_split_prompt_prefixes_and_route_to_providers() {
        assert_eq!(
            TaskClass::split_prefix("/deep why does this leak?"),
            (Some(TaskClass::Deep), "why does this leak?")
        );
        assert_eq!(TaskClass::split_prefix("/deepdive"), (None, "/deepdive"));
        assert_eq!(TaskClass::split_prefix("hola"), (None, "hola"));
        let router = RouterConfig {
            trivial: "local".into(),
            ..RouterConfig::default()
        };
        assert_eq!(router.provider_for(TaskClass::Trivial), Some("local"));
        assert_eq!(router.provider_for(TaskClass::Deep), None);
        let provider = ProviderConfig {
            name: "local".into(),
            kind: ProviderKind::OpenaiCompatible,
            model: "qwen".into(),
            base_url: "http://localhost:11434/v1".into(),
            free: true,
            ..ProviderConfig::default()
        };
        let env = provider.environment();
        assert!(env.contains(&("OPENAI_BASE_URL".into(), "http://localhost:11434/v1".into())));
        assert!(env.contains(&("OPENAI_MODEL".into(), "qwen".into())));
        assert!(
            !env.iter().any(|(k, _)| k == "OPENAI_API_KEY"),
            "no key env, no key"
        );
        assert_eq!(provider.describe(), "local: openai-compatible:qwen (free)");
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
        assert!(
            Config::default_path(None).is_none_or(|path| path.ends_with("forge/config.toml")
                || std::env::var_os("FORGE_CONFIG").is_some())
        );
    }

    #[test]
    fn json_schema_documents_every_section() {
        let schema = Config::json_schema();
        for key in [
            "font",
            "colors",
            "terminal",
            "ui",
            "keybindings",
            "HexColor",
        ] {
            assert!(schema.contains(key), "schema lacks {key}");
        }
    }
}
