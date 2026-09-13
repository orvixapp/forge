//! Pure state for the Phase 1 application shell.
//!
//! Widgets ask this module which command a keystroke represents; command
//! execution remains at the application boundary, where it can create windows
//! or update layout state without coupling the resolver to GPUI.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::{fs, io, path::Path};

/// Stable identifiers used by keymaps, the future command palette and CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellCommand {
    NewTerminalWindow,
    CloseWindow,
    ToggleMaximize,
    ShowCommandPalette,
    SplitHorizontal,
    SplitVertical,
    FocusNextPane,
}

impl ShellCommand {
    pub const ALL: [Self; 7] = [
        Self::NewTerminalWindow, Self::CloseWindow, Self::ToggleMaximize,
        Self::ShowCommandPalette, Self::SplitHorizontal, Self::SplitVertical,
        Self::FocusNextPane,
    ];
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::NewTerminalWindow => "window.newTerminal",
            Self::CloseWindow => "window.close",
            Self::ToggleMaximize => "window.toggleMaximize",
            Self::ShowCommandPalette => "commandPalette.show",
            Self::SplitHorizontal => "layout.splitHorizontal",
            Self::SplitVertical => "layout.splitVertical",
            Self::FocusNextPane => "layout.focusNextPane",
        }
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::NewTerminalWindow => "New terminal window",
            Self::CloseWindow => "Close window",
            Self::ToggleMaximize => "Toggle maximized window",
            Self::ShowCommandPalette => "Show command palette",
            Self::SplitHorizontal => "Split horizontally",
            Self::SplitVertical => "Split vertically",
            Self::FocusNextPane => "Focus next pane",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteMatch { pub command: ShellCommand, pub score: usize }

#[must_use]
pub fn search_commands(query: &str) -> Vec<PaletteMatch> {
    let query = query.trim().to_ascii_lowercase();
    let mut matches: Vec<_> = ShellCommand::ALL.into_iter().filter_map(|command| {
        fuzzy_score(&query, command.title()).or_else(|| fuzzy_score(&query, command.id()))
            .map(|score| PaletteMatch { command, score })
    }).collect();
    matches.sort_by_key(|item| (item.score, item.command.title()));
    matches
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    if query.is_empty() { return Some(0); }
    let mut chars = candidate.to_ascii_lowercase().chars();
    let mut skipped = 0;
    for wanted in query.chars() {
        loop {
            match chars.next() {
                Some(found) if found == wanted => break,
                Some(_) => skipped += 1,
                None => return None,
            }
        }
    }
    Some(skipped)
}

/// A context is intentionally small at first. Future panes add their own
/// context without making terminal keystrokes accidentally trigger shell UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellContext {
    Window,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ShellKeystroke {
    pub key: String,
    pub control: bool,
    pub alt: bool,
    pub shift: bool,
}

impl ShellKeystroke {
    #[must_use]
    pub fn new(key: impl AsRef<str>, control: bool, alt: bool, shift: bool) -> Self {
        Self {
            key: key.as_ref().to_ascii_lowercase(),
            control,
            alt,
            shift,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyBinding {
    pub keystroke: ShellKeystroke,
    pub context: ShellContext,
    pub command: ShellCommand,
}

/// Ordered keymap. Later bindings override earlier bindings, which gives user
/// configuration a deterministic override mechanism in subphase 1.4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellKeymap {
    bindings: Vec<KeyBinding>,
}

/// Content-free pane types. Terminal/editor implementations plug into these
/// stable IDs in later phases without changing persisted layouts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneKind {
    Empty,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pane {
    pub id: u64,
    pub kind: PaneKind,
    pub title: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// A recursive shell layout: leaves are panes, and groups are tabs or splits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LayoutNode {
    Pane(Pane),
    Tabs {
        active: usize,
        children: Vec<LayoutNode>,
    },
    Split {
        direction: SplitDirection,
        ratio: f32,
        first: Box<LayoutNode>,
        second: Box<LayoutNode>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellLayout {
    pub root: LayoutNode,
    focused_pane: u64,
    next_pane_id: u64,
}

impl Default for ShellLayout {
    fn default() -> Self {
        let pane = Pane {
            id: 1,
            kind: PaneKind::Empty,
            title: "Welcome".into(),
        };
        Self {
            root: LayoutNode::Tabs {
                active: 0,
                children: vec![LayoutNode::Pane(pane)],
            },
            focused_pane: 1,
            next_pane_id: 2,
        }
    }
}

impl ShellLayout {
    #[must_use]
    pub const fn focused_pane(&self) -> u64 {
        self.focused_pane
    }

    #[must_use]
    pub fn pane_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        collect_panes(&self.root, &mut ids);
        ids
    }

    /// Splits the focused leaf and focuses the newly-created empty pane.
    pub fn split_focused(&mut self, direction: SplitDirection) {
        let old = self.focused_pane;
        let new = Pane {
            id: self.next_pane_id,
            kind: PaneKind::Empty,
            title: "Empty pane".into(),
        };
        self.next_pane_id += 1;
        if split_pane(&mut self.root, old, direction, new.clone()) {
            self.focused_pane = new.id;
        }
    }

    pub fn focus_next(&mut self) {
        let ids = self.pane_ids();
        if let Some(index) = ids.iter().position(|id| *id == self.focused_pane) {
            self.focused_pane = ids[(index + 1) % ids.len()];
        }
    }
}

fn collect_panes(node: &LayoutNode, output: &mut Vec<u64>) {
    match node {
        LayoutNode::Pane(pane) => output.push(pane.id),
        LayoutNode::Tabs { children, .. } => children
            .iter()
            .for_each(|child| collect_panes(child, output)),
        LayoutNode::Split { first, second, .. } => {
            collect_panes(first, output);
            collect_panes(second, output);
        }
    }
}

fn split_pane(node: &mut LayoutNode, id: u64, direction: SplitDirection, new: Pane) -> bool {
    match node {
        LayoutNode::Pane(pane) if pane.id == id => {
            let old = std::mem::replace(node, LayoutNode::Pane(new));
            *node = LayoutNode::Split {
                direction,
                ratio: 0.5,
                first: Box::new(old),
                second: Box::new(match node {
                    LayoutNode::Pane(pane) => LayoutNode::Pane(pane.clone()),
                    _ => unreachable!(),
                }),
            };
            true
        }
        LayoutNode::Tabs { children, .. } => children
            .iter_mut()
            .any(|child| split_pane(child, id, direction, new.clone())),
        LayoutNode::Split { first, second, .. } => {
            split_pane(first, id, direction, new.clone()) || split_pane(second, id, direction, new)
        }
        LayoutNode::Pane(_) => false,
    }
}

/// Persisted, versioned shell state. Invalid or future state is rejected by
/// callers so a broken cache can never stop Forge from opening.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShellSession {
    pub version: u8,
    pub layout: ShellLayout,
    pub theme: String,
}

impl Default for ShellSession {
    fn default() -> Self {
        Self {
            version: 1,
            layout: ShellLayout::default(),
            theme: "forge-dark".into(),
        }
    }
}

impl ShellSession {
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = fs::read_to_string(path)?;
        let session: Self = serde_json::from_str(&strip_jsonc_comments(&text))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if session.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported shell session version",
            ));
        }
        Ok(session)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "session path has no parent")
        })?;
        fs::create_dir_all(parent)?;
        let temporary = path.with_extension("tmp");
        fs::write(
            &temporary,
            serde_json::to_string_pretty(self).expect("session is serializable"),
        )?;
        fs::rename(temporary, path)
    }
}

/// Removes line and block comments while preserving quoted strings. JSONC
/// trailing commas are deliberately not accepted yet: rejecting malformed
/// configuration is safer than silently changing its meaning.
fn strip_jsonc_comments(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut quoted = false;
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if quoted {
            escaped = ch == '\\' && !escaped;
            if ch == '"' && !escaped {
                quoted = false;
            }
            out.push(ch);
            continue;
        }
        if ch == '"' {
            quoted = true;
            out.push(ch);
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            chars.next();
            for line in chars.by_ref() {
                if line == '\n' {
                    out.push('\n');
                    break;
                }
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'*') {
            chars.next();
            while let Some(block) = chars.next() {
                if block == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    break;
                }
            }
            continue;
        }
        out.push(ch);
    }
    out
}

/// Minimal Phase 1 settings. More settings are added by their owning phase;
/// unknown fields remain in the JSON layer rather than silently affecting UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShellSettings {
    pub theme: String,
}

impl Default for ShellSettings {
    fn default() -> Self {
        Self {
            theme: "forge-dark".into(),
        }
    }
}

/// Reads JSONC layers in ascending precedence (defaults, user, workspace).
/// Missing layers are ignored; a malformed present layer is an error and must
/// leave the already-running configuration untouched.
pub fn load_jsonc_layers<T: DeserializeOwned + Default>(paths: &[&Path]) -> io::Result<T> {
    let mut merged = Value::Object(Default::default());
    for path in paths {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let layer: Value = serde_json::from_str(&strip_jsonc_comments(&text)).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: {error}", path.display()),
            )
        })?;
        merge_json(&mut merged, layer);
    }
    serde_json::from_value(merged)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn merge_json(base: &mut Value, override_value: Value) {
    match (base, override_value) {
        (Value::Object(base), Value::Object(override_value)) => {
            for (key, value) in override_value {
                merge_json(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, override_value) => *base = override_value,
    }
}

impl Default for ShellKeymap {
    fn default() -> Self {
        use ShellCommand::*;
        use ShellContext::*;
        Self {
            bindings: vec![
                binding("t", true, false, false, Terminal, NewTerminalWindow),
                binding("p", true, false, true, Window, ShowCommandPalette),
                binding("w", true, false, false, Window, CloseWindow),
                binding("\\", true, false, false, Window, SplitVertical),
                binding("5", true, false, true, Window, SplitHorizontal),
                binding("tab", true, false, false, Window, FocusNextPane),
            ],
        }
    }
}

impl ShellKeymap {
    #[must_use]
    pub fn resolve(
        &self,
        keystroke: &ShellKeystroke,
        context: ShellContext,
    ) -> Option<ShellCommand> {
        self.bindings
            .iter()
            .rev()
            .find(|binding| binding.context == context && binding.keystroke == *keystroke)
            .map(|binding| binding.command)
            .or_else(|| {
                (context != ShellContext::Window)
                    .then(|| {
                        self.bindings
                            .iter()
                            .rev()
                            .find(|binding| {
                                binding.context == ShellContext::Window
                                    && binding.keystroke == *keystroke
                            })
                            .map(|binding| binding.command)
                    })
                    .flatten()
            })
    }

    pub fn bind(&mut self, binding: KeyBinding) {
        self.bindings.push(binding);
    }
}

fn binding(
    key: &str,
    control: bool,
    alt: bool,
    shift: bool,
    context: ShellContext,
    command: ShellCommand,
) -> KeyBinding {
    KeyBinding {
        keystroke: ShellKeystroke::new(key, control, alt, shift),
        context,
        command,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_shortcut_resolves_to_a_stable_command() {
        let keymap = ShellKeymap::default();
        assert_eq!(
            keymap.resolve(
                &ShellKeystroke::new("T", true, false, false),
                ShellContext::Terminal
            ),
            Some(ShellCommand::NewTerminalWindow)
        );
        assert_eq!(ShellCommand::NewTerminalWindow.id(), "window.newTerminal");
    }

    #[test]
    fn window_commands_are_available_inside_a_terminal() {
        let keymap = ShellKeymap::default();
        assert_eq!(
            keymap.resolve(
                &ShellKeystroke::new("p", true, false, true),
                ShellContext::Terminal
            ),
            Some(ShellCommand::ShowCommandPalette)
        );
    }

    #[test]
    fn later_binding_wins_in_the_same_context() {
        let mut keymap = ShellKeymap::default();
        keymap.bind(binding(
            "t",
            true,
            false,
            false,
            ShellContext::Terminal,
            ShellCommand::ShowCommandPalette,
        ));
        assert_eq!(
            keymap.resolve(
                &ShellKeystroke::new("t", true, false, false),
                ShellContext::Terminal
            ),
            Some(ShellCommand::ShowCommandPalette)
        );
    }

    #[test]
    fn splitting_and_focusing_produces_a_stable_layout_tree() {
        let mut layout = ShellLayout::default();
        layout.split_focused(SplitDirection::Vertical);
        assert_eq!(layout.pane_ids(), vec![1, 2]);
        assert_eq!(layout.focused_pane(), 2);
        layout.focus_next();
        assert_eq!(layout.focused_pane(), 1);
    }

    #[test]
    fn session_round_trips_jsonc_comments() {
        let path = std::env::temp_dir().join(format!("forge-session-{}.jsonc", std::process::id()));
        let session = ShellSession::default();
        session.save(&path).unwrap();
        let json = fs::read_to_string(&path).unwrap();
        fs::write(&path, format!("// Forge session\n{json}\n/* end */")).unwrap();
        assert_eq!(ShellSession::load(&path).unwrap(), session);
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn jsonc_layers_merge_in_precedence_order() {
        let root = std::env::temp_dir().join(format!("forge-settings-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let user = root.join("user.jsonc");
        let workspace = root.join("workspace.jsonc");
        fs::write(&user, "// user\n{ \"theme\": \"forge-light\" }").unwrap();
        fs::write(
            &workspace,
            "{ /* workspace wins */ \"theme\": \"forge-dark\" }",
        )
        .unwrap();
        let settings: ShellSettings = load_jsonc_layers(&[&user, &workspace]).unwrap();
        assert_eq!(settings.theme, "forge-dark");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn palette_fuzzy_search_finds_new_terminal() {
        assert_eq!(search_commands("new term").first().map(|item| item.command), Some(ShellCommand::NewTerminalWindow));
        assert!(search_commands("not a command").is_empty());
    }
}
