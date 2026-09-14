//! Pure state for the application shell: the command registry, the keymap,
//! the pane tree and the persisted window session.
//!
//! Widgets ask this module which command a keystroke represents; command
//! execution stays at the application boundary, where it can create windows
//! or update layout state without coupling the resolver to GPUI.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

/// Stable identifiers used by keymaps, the command palette and the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellCommand {
    NewTerminalTab,
    NewTerminalTabInDirectory,
    CloseWindow,
    ToggleMaximize,
    ShowCommandPalette,
    SplitHorizontal,
    SplitVertical,
    FocusNextPane,
    ShowProcessExplorer,
    CycleTheme,
    ReloadConfig,
}

impl ShellCommand {
    pub const ALL: [Self; 11] = [
        Self::NewTerminalTab,
        Self::NewTerminalTabInDirectory,
        Self::CloseWindow,
        Self::ToggleMaximize,
        Self::ShowCommandPalette,
        Self::SplitHorizontal,
        Self::SplitVertical,
        Self::FocusNextPane,
        Self::ShowProcessExplorer,
        Self::CycleTheme,
        Self::ReloadConfig,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::NewTerminalTab => "terminal.newTab",
            Self::NewTerminalTabInDirectory => "terminal.newTabInDirectory",
            Self::CloseWindow => "window.close",
            Self::ToggleMaximize => "window.toggleMaximize",
            Self::ShowCommandPalette => "commandPalette.show",
            Self::SplitHorizontal => "layout.splitHorizontal",
            Self::SplitVertical => "layout.splitVertical",
            Self::FocusNextPane => "layout.focusNextPane",
            Self::ShowProcessExplorer => "processExplorer.show",
            Self::CycleTheme => "theme.cycle",
            Self::ReloadConfig => "config.reload",
        }
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::NewTerminalTab => "New terminal tab",
            Self::NewTerminalTabInDirectory => "New terminal tab in directory…",
            Self::CloseWindow => "Close tab or window",
            Self::ToggleMaximize => "Toggle maximized window",
            Self::ShowCommandPalette => "Show command palette",
            Self::SplitHorizontal => "Split horizontally",
            Self::SplitVertical => "Split vertically",
            Self::FocusNextPane => "Focus next pane",
            Self::ShowProcessExplorer => "Show process explorer",
            Self::CycleTheme => "Cycle theme",
            Self::ReloadConfig => "Reload configuration",
        }
    }

    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|command| command.id() == id)
    }
}

/// User-owned binding declaration, accepted from `config.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UserKeyBinding {
    /// Command id, e.g. `terminal.newTab`.
    pub command: String,
    /// Chord such as `ctrl+shift+p`.
    pub keys: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaletteMatch {
    pub command: ShellCommand,
    pub score: usize,
}

#[must_use]
pub fn search_commands(query: &str) -> Vec<PaletteMatch> {
    let query = query.trim().to_ascii_lowercase();
    let mut matches: Vec<_> = ShellCommand::ALL
        .into_iter()
        .filter_map(|command| {
            fuzzy_score(&query, command.title())
                .or_else(|| fuzzy_score(&query, command.id()))
                .map(|score| PaletteMatch { command, score })
        })
        .collect();
    matches.sort_by_key(|item| (item.score, item.command.title()));
    matches
}

fn fuzzy_score(query: &str, candidate: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let lowercase = candidate.to_ascii_lowercase();
    let mut chars = lowercase.chars();
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
/// configuration a deterministic override mechanism.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellKeymap {
    bindings: Vec<KeyBinding>,
}

impl Default for ShellKeymap {
    fn default() -> Self {
        use ShellCommand::{
            CloseWindow, CycleTheme, FocusNextPane, NewTerminalTab, ShowCommandPalette,
            SplitHorizontal, SplitVertical,
        };
        use ShellContext::{Terminal, Window};
        Self {
            bindings: vec![
                binding("t", true, false, false, Terminal, NewTerminalTab),
                binding("p", true, false, true, Window, ShowCommandPalette),
                binding("w", true, false, false, Window, CloseWindow),
                binding("\\", true, false, false, Window, SplitVertical),
                binding("5", true, false, true, Window, SplitHorizontal),
                binding("tab", true, false, false, Window, FocusNextPane),
                binding("t", true, false, true, Window, CycleTheme),
            ],
        }
    }
}

impl ShellKeymap {
    #[must_use]
    pub fn with_overrides(mut self, overrides: &[UserKeyBinding]) -> Self {
        for override_ in overrides {
            let Some(command) = ShellCommand::from_id(&override_.command) else {
                continue;
            };
            let Some(keystroke) = parse_keybinding(&override_.keys) else {
                continue;
            };
            self.bindings
                .retain(|binding| binding.command != command && binding.keystroke != keystroke);
            self.bindings.push(KeyBinding {
                keystroke,
                context: ShellContext::Window,
                command,
            });
        }
        self
    }

    /// Resolves a keystroke in `context`; `Window` bindings apply inside every
    /// other context unless that context binds the same keystroke.
    #[must_use]
    pub fn resolve(
        &self,
        keystroke: &ShellKeystroke,
        context: ShellContext,
    ) -> Option<ShellCommand> {
        let in_context = |wanted: ShellContext| {
            self.bindings
                .iter()
                .rev()
                .find(|binding| binding.context == wanted && binding.keystroke == *keystroke)
                .map(|binding| binding.command)
        };
        in_context(context).or_else(|| {
            (context != ShellContext::Window)
                .then(|| in_context(ShellContext::Window))
                .flatten()
        })
    }

    pub fn bind(&mut self, binding: KeyBinding) {
        self.bindings.push(binding);
    }

    /// The first chord bound to `command`, for hints in the UI.
    #[must_use]
    pub fn chord_for(&self, command: ShellCommand) -> Option<String> {
        self.bindings
            .iter()
            .rev()
            .find(|binding| binding.command == command)
            .map(|binding| {
                let key = &binding.keystroke;
                let mut parts = Vec::new();
                if key.control {
                    parts.push("Ctrl".to_string());
                }
                if key.alt {
                    parts.push("Alt".to_string());
                }
                if key.shift {
                    parts.push("Shift".to_string());
                }
                let mut name = key.key.clone();
                if let Some(first) = name.get(..1) {
                    name = first.to_ascii_uppercase() + &name[1..];
                }
                parts.push(name);
                parts.join("+")
            })
    }
}

fn parse_keybinding(value: &str) -> Option<ShellKeystroke> {
    let mut control = false;
    let mut alt = false;
    let mut shift = false;
    let mut key = None;
    for part in value
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => control = true,
            "alt" => alt = true,
            "shift" => shift = true,
            candidate if key.is_none() => key = Some(candidate.to_owned()),
            _ => return None,
        }
    }
    key.map(|key| ShellKeystroke::new(key, control, alt, shift))
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

/// Axis-aligned rectangle in logical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitDirection {
    Horizontal,
    Vertical,
}

/// Binary split tree over tab indices. Leaves are indices into the window's
/// tab list; removing a tab shifts the indices above it down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneTree {
    Leaf {
        index: usize,
    },
    Split {
        direction: SplitDirection,
        first: Box<PaneTree>,
        second: Box<PaneTree>,
    },
}

impl PaneTree {
    #[must_use]
    pub const fn leaf(index: usize) -> Self {
        Self::Leaf { index }
    }

    /// Replaces the leaf `target` with a split of `target` and `new`.
    pub fn split(&mut self, target: usize, new: usize, direction: SplitDirection) {
        match self {
            Self::Leaf { index } if *index == target => {
                *self = Self::Split {
                    direction,
                    first: Box::new(Self::leaf(target)),
                    second: Box::new(Self::leaf(new)),
                };
            }
            Self::Split { first, second, .. } => {
                first.split(target, new, direction);
                second.split(target, new, direction);
            }
            Self::Leaf { .. } => {}
        }
    }

    #[must_use]
    pub fn leaves(&self) -> Vec<usize> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<usize>) {
        match self {
            Self::Leaf { index } => out.push(*index),
            Self::Split { first, second, .. } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    #[must_use]
    pub fn contains(&self, index: usize) -> bool {
        self.leaves().contains(&index)
    }

    /// Removes tab `target`, collapsing its parent split, and renumbers the
    /// leaves above it. `None` when the tree becomes empty.
    #[must_use]
    pub fn remove(self, target: usize) -> Option<Self> {
        match self {
            Self::Leaf { index } if index == target => None,
            Self::Leaf { index } => {
                Some(Self::leaf(if index > target { index - 1 } else { index }))
            }
            Self::Split {
                direction,
                first,
                second,
            } => match (first.remove(target), second.remove(target)) {
                (Some(first), Some(second)) => Some(Self::Split {
                    direction,
                    first: Box::new(first),
                    second: Box::new(second),
                }),
                (remaining, None) | (None, remaining) => remaining,
            },
        }
    }

    /// Rectangles of every leaf inside `area`, splitting each node in half
    /// with `gap` pixels between the halves. Computed here rather than with
    /// nested flex containers because flex layout cost grows with nesting
    /// depth, and a terminal multiplexer wants explicit geometry anyway.
    #[must_use]
    pub fn layout(&self, area: Rect, gap: f32) -> Vec<(usize, Rect)> {
        let mut out = Vec::new();
        self.layout_into(area, gap, &mut out);
        out
    }

    fn layout_into(&self, area: Rect, gap: f32, out: &mut Vec<(usize, Rect)>) {
        match self {
            Self::Leaf { index } => out.push((*index, area)),
            Self::Split {
                direction,
                first,
                second,
            } => {
                let (a, b) = match direction {
                    SplitDirection::Vertical => {
                        let width = ((area.width - gap) / 2.0).max(0.0);
                        (
                            Rect { width, ..area },
                            Rect {
                                x: area.x + width + gap,
                                width,
                                ..area
                            },
                        )
                    }
                    SplitDirection::Horizontal => {
                        let height = ((area.height - gap) / 2.0).max(0.0);
                        (
                            Rect { height, ..area },
                            Rect {
                                y: area.y + height + gap,
                                height,
                                ..area
                            },
                        )
                    }
                };
                first.layout_into(a, gap, out);
                second.layout_into(b, gap, out);
            }
        }
    }

    /// Leaf after `current` in reading order, wrapping around.
    #[must_use]
    pub fn next_leaf(&self, current: usize) -> usize {
        let leaves = self.leaves();
        let position = leaves.iter().position(|leaf| *leaf == current).unwrap_or(0);
        leaves[(position + 1) % leaves.len()]
    }
}

/// Persisted, versioned window state. Invalid or future state is rejected so
/// a broken file can never stop Forge from opening.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WindowSession {
    pub version: u8,
    /// Number of terminal tabs.
    pub count: usize,
    pub active: usize,
    pub split: Option<PaneTree>,
    pub cwd: std::path::PathBuf,
    pub width: f32,
    pub height: f32,
    #[serde(default)]
    pub theme: Option<String>,
    /// Daemon session behind each tab, so a restart reattaches instead of
    /// spawning new shells. Shorter than `count` for files written before
    /// this field existed.
    #[serde(default)]
    pub sessions: Vec<Option<u64>>,
}

impl WindowSession {
    pub const VERSION: u8 = 2;
    pub const MAX_TABS: usize = 64;

    /// Reads and validates a session file; `Ok(None)` when it does not exist.
    ///
    /// # Errors
    ///
    /// A present but unreadable, unparsable or inconsistent file.
    pub fn load(path: &Path) -> io::Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let session: Self = serde_json::from_str(&text)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        session
            .validate()
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
        Ok(Some(session))
    }

    /// Writes atomically and only when the content changed, so a periodic
    /// save never touches the disk while the layout is stable.
    ///
    /// # Errors
    ///
    /// I/O failures creating the directory or replacing the file.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self)?;
        if fs::read(path).ok().as_deref() == Some(bytes.as_slice()) {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("json.tmp");
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, path)
    }

    /// Structural checks that keep a corrupt file from producing an
    /// out-of-range tab index at runtime.
    ///
    /// # Errors
    ///
    /// A human-readable reason.
    pub fn validate(&self) -> Result<(), String> {
        if self.version != Self::VERSION {
            return Err(format!("unsupported session version {}", self.version));
        }
        if self.count == 0 || self.count > Self::MAX_TABS {
            return Err(format!("tab count {} out of range", self.count));
        }
        if self.active >= self.count {
            return Err("active tab out of range".into());
        }
        if let Some(tree) = &self.split {
            let leaves = tree.leaves();
            if leaves.iter().any(|index| *index >= self.count) {
                return Err("split references a missing tab".into());
            }
            let mut sorted = leaves.clone();
            sorted.sort_unstable();
            sorted.dedup();
            if sorted.len() != leaves.len() {
                return Err("split references a tab twice".into());
            }
        }
        if !(self.width.is_finite() && self.height.is_finite()) {
            return Err("window size is not finite".into());
        }
        if self.sessions.len() > self.count {
            return Err("more daemon sessions than tabs".into());
        }
        Ok(())
    }

    /// Daemon session for tab `index`, if the file recorded one.
    #[must_use]
    pub fn daemon_session(&self, index: usize) -> Option<u64> {
        self.sessions.get(index).copied().flatten()
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
            Some(ShellCommand::NewTerminalTab)
        );
        assert_eq!(ShellCommand::NewTerminalTab.id(), "terminal.newTab");
        assert_eq!(
            keymap
                .chord_for(ShellCommand::ShowCommandPalette)
                .as_deref(),
            Some("Ctrl+Shift+P")
        );
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
    fn user_keybinding_overrides_the_default_and_ignores_garbage() {
        let keymap = ShellKeymap::default().with_overrides(&[
            UserKeyBinding {
                command: "terminal.newTab".into(),
                keys: "ctrl+n".into(),
            },
            UserKeyBinding {
                command: "no.such.command".into(),
                keys: "ctrl+x".into(),
            },
            UserKeyBinding {
                command: "theme.cycle".into(),
                keys: "hyper+t".into(),
            },
        ]);
        assert_eq!(
            keymap.resolve(
                &ShellKeystroke::new("n", true, false, false),
                ShellContext::Terminal
            ),
            Some(ShellCommand::NewTerminalTab)
        );
        assert_eq!(
            keymap.resolve(
                &ShellKeystroke::new("t", true, false, false),
                ShellContext::Terminal
            ),
            None
        );
        assert_eq!(
            keymap.resolve(
                &ShellKeystroke::new("t", true, false, true),
                ShellContext::Window
            ),
            Some(ShellCommand::CycleTheme)
        );
    }

    #[test]
    fn every_command_has_a_unique_id_and_round_trips() {
        let mut ids: Vec<_> = ShellCommand::ALL
            .iter()
            .map(|command| command.id())
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), ShellCommand::ALL.len());
        for command in ShellCommand::ALL {
            assert_eq!(ShellCommand::from_id(command.id()), Some(command));
        }
    }

    #[test]
    fn palette_fuzzy_search_finds_new_terminal() {
        assert_eq!(
            search_commands("new term").first().map(|item| item.command),
            Some(ShellCommand::NewTerminalTab)
        );
        assert!(search_commands("not a command").is_empty());
    }

    #[test]
    fn nested_split_close_preserves_remaining_branches() {
        let mut tree = PaneTree::leaf(0);
        tree.split(0, 1, SplitDirection::Vertical);
        tree.split(1, 2, SplitDirection::Horizontal);
        tree.split(2, 3, SplitDirection::Vertical);
        assert_eq!(tree.leaves(), [0, 1, 2, 3]);
        assert_eq!(tree.next_leaf(3), 0);
        let tree = tree.remove(1).unwrap();
        assert_eq!(tree.leaves(), [0, 1, 2]);
        assert!(matches!(tree, PaneTree::Split { .. }));
        let encoded = serde_json::to_vec(&tree).unwrap();
        let restored: PaneTree = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(restored, tree);
        assert_eq!(PaneTree::leaf(0).remove(0), None);
    }

    #[test]
    fn layout_halves_the_area_and_keeps_the_gap() {
        let mut tree = PaneTree::leaf(0);
        tree.split(0, 1, SplitDirection::Vertical);
        tree.split(1, 2, SplitDirection::Horizontal);
        let area = Rect {
            x: 10.0,
            y: 20.0,
            width: 202.0,
            height: 102.0,
        };
        let rects = tree.layout(area, 2.0);
        assert_eq!(rects.len(), 3);
        assert_eq!(
            rects[0],
            (
                0,
                Rect {
                    x: 10.0,
                    y: 20.0,
                    width: 100.0,
                    height: 102.0
                }
            )
        );
        assert_eq!(
            rects[1],
            (
                1,
                Rect {
                    x: 112.0,
                    y: 20.0,
                    width: 100.0,
                    height: 50.0
                }
            )
        );
        assert_eq!(
            rects[2],
            (
                2,
                Rect {
                    x: 112.0,
                    y: 72.0,
                    width: 100.0,
                    height: 50.0
                }
            )
        );
        let tiny = tree.layout(Rect::default(), 2.0);
        assert!(
            tiny.iter()
                .all(|(_, rect)| rect.width >= 0.0 && rect.height >= 0.0)
        );
    }

    #[test]
    fn session_round_trips_and_rejects_inconsistent_state() {
        let path = std::env::temp_dir().join(format!("forge-session-{}.json", std::process::id()));
        let mut tree = PaneTree::leaf(0);
        tree.split(0, 1, SplitDirection::Horizontal);
        let session = WindowSession {
            version: WindowSession::VERSION,
            count: 2,
            active: 1,
            split: Some(tree),
            cwd: std::env::temp_dir(),
            width: 960.0,
            height: 600.0,
            theme: Some("forge-light".into()),
            sessions: vec![Some(7), None],
        };
        session.save(&path).unwrap();
        assert_eq!(WindowSession::load(&path).unwrap(), Some(session.clone()));
        // Unchanged content does not rewrite the file.
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        session.save(&path).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), modified);

        let broken = WindowSession {
            active: 5,
            ..session.clone()
        };
        assert!(broken.validate().is_err());
        let stale = WindowSession {
            version: 1,
            ..session.clone()
        };
        assert!(stale.validate().is_err());
        assert_eq!(session.daemon_session(0), Some(7));
        assert_eq!(session.daemon_session(1), None);
        assert_eq!(session.daemon_session(5), None);
        let too_many = WindowSession {
            sessions: vec![None; 3],
            ..session
        };
        assert!(too_many.validate().is_err());
        fs::write(&path, "{not json").unwrap();
        assert!(WindowSession::load(&path).is_err());
        fs::remove_file(&path).unwrap();
        assert_eq!(WindowSession::load(&path).unwrap(), None);
    }
}
