//! Colour themes for the shell and the terminal grid.
//!
//! A theme is a complete set of colours. Built-in themes ship with Forge;
//! users add `<config dir>/themes/<name>.toml` files that override any subset
//! of a built-in theme, and `[colors]` in `config.toml` overrides single keys
//! on top of the selected theme.

use crate::config::{ColorOverrides, HexColor};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// Concrete colours after applying theme file and config overrides.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ThemeColors {
    pub background: HexColor,
    pub foreground: HexColor,
    pub cursor: HexColor,
    pub selection: HexColor,
    /// Opacity of the selection overlay, 0–1.
    pub selection_opacity: f32,
    /// Status text and other secondary accents.
    pub accent: HexColor,
    /// Top bar and overlays.
    pub chrome: HexColor,
    pub chrome_border: HexColor,
    /// Active tab, hovered buttons.
    pub chrome_active: HexColor,
    pub chrome_active_border: HexColor,
    /// Secondary text in the chrome.
    pub muted: HexColor,
    /// Selected palette row.
    pub highlight: HexColor,
    /// Close-window hover.
    pub danger: HexColor,
    /// Background of scrollback search matches.
    pub search_match: HexColor,
    /// Background of the selected search match.
    pub search_current: HexColor,
    pub syntax: SyntaxColors,
}

/// Colours of the editor's syntax tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SyntaxColors {
    pub keyword: HexColor,
    pub string: HexColor,
    pub comment: HexColor,
    pub function: HexColor,
    pub type_: HexColor,
    pub variable: HexColor,
    pub number: HexColor,
    pub constant: HexColor,
    pub operator: HexColor,
    pub punctuation: HexColor,
    pub attribute: HexColor,
    pub property: HexColor,
    pub tag: HexColor,
}

/// `[syntax]` table of a theme file; every key optional.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyntaxFile {
    pub keyword: Option<HexColor>,
    pub string: Option<HexColor>,
    pub comment: Option<HexColor>,
    pub function: Option<HexColor>,
    #[serde(rename = "type")]
    pub type_: Option<HexColor>,
    pub variable: Option<HexColor>,
    pub number: Option<HexColor>,
    pub constant: Option<HexColor>,
    pub operator: Option<HexColor>,
    pub punctuation: Option<HexColor>,
    pub attribute: Option<HexColor>,
    pub property: Option<HexColor>,
    pub tag: Option<HexColor>,
}

/// Every field optional so a theme file can override just a few colours.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThemeFile {
    /// Built-in theme this file starts from; defaults to `forge-dark`.
    pub base: Option<String>,
    pub background: Option<HexColor>,
    pub foreground: Option<HexColor>,
    pub cursor: Option<HexColor>,
    pub selection: Option<HexColor>,
    pub selection_opacity: Option<f32>,
    pub accent: Option<HexColor>,
    pub chrome: Option<HexColor>,
    pub chrome_border: Option<HexColor>,
    pub chrome_active: Option<HexColor>,
    pub chrome_active_border: Option<HexColor>,
    pub muted: Option<HexColor>,
    pub highlight: Option<HexColor>,
    pub danger: Option<HexColor>,
    pub search_match: Option<HexColor>,
    pub search_current: Option<HexColor>,
    pub syntax: SyntaxFile,
}

pub const FORGE_DARK: &str = "forge-dark";
pub const FORGE_LIGHT: &str = "forge-light";
pub const BUILTIN_THEMES: &[&str] = &[FORGE_DARK, FORGE_LIGHT];

impl ThemeColors {
    #[must_use]
    pub const fn forge_dark() -> Self {
        Self {
            background: HexColor::new(0x11, 0x13, 0x18),
            foreground: HexColor::new(0xd8, 0xde, 0xe9),
            cursor: HexColor::new(0xeb, 0xcb, 0x8b),
            selection: HexColor::new(0x88, 0xc0, 0xd0),
            selection_opacity: 0.35,
            accent: HexColor::new(0x88, 0xc0, 0xd0),
            chrome: HexColor::new(0x17, 0x1b, 0x24),
            chrome_border: HexColor::new(0x2a, 0x31, 0x40),
            chrome_active: HexColor::new(0x25, 0x2c, 0x3a),
            chrome_active_border: HexColor::new(0x3b, 0x46, 0x5c),
            muted: HexColor::new(0x7f, 0x8a, 0xa3),
            highlight: HexColor::new(0x3b, 0x52, 0x6e),
            danger: HexColor::new(0xa8, 0x41, 0x52),
            search_match: HexColor::new(0x5c, 0x4a, 0x1a),
            search_current: HexColor::new(0xb5, 0x8a, 0x2e),
            syntax: SyntaxColors {
                keyword: HexColor::new(0x81, 0xa1, 0xc1),
                string: HexColor::new(0xa3, 0xbe, 0x8c),
                comment: HexColor::new(0x61, 0x6e, 0x88),
                function: HexColor::new(0x88, 0xc0, 0xd0),
                type_: HexColor::new(0x8f, 0xbc, 0xbb),
                variable: HexColor::new(0xd8, 0xde, 0xe9),
                number: HexColor::new(0xb4, 0x8e, 0xad),
                constant: HexColor::new(0xd0, 0x87, 0x70),
                operator: HexColor::new(0x81, 0xa1, 0xc1),
                punctuation: HexColor::new(0xa5, 0xad, 0xbd),
                attribute: HexColor::new(0xd0, 0x87, 0x70),
                property: HexColor::new(0x8f, 0xbc, 0xbb),
                tag: HexColor::new(0xeb, 0xcb, 0x8b),
            },
        }
    }

    #[must_use]
    pub const fn forge_light() -> Self {
        Self {
            background: HexColor::new(0xfa, 0xfa, 0xf7),
            foreground: HexColor::new(0x2e, 0x34, 0x40),
            cursor: HexColor::new(0xd0, 0x87, 0x70),
            selection: HexColor::new(0x5e, 0x81, 0xac),
            selection_opacity: 0.25,
            accent: HexColor::new(0x5e, 0x81, 0xac),
            chrome: HexColor::new(0xec, 0xee, 0xf2),
            chrome_border: HexColor::new(0xd4, 0xd8, 0xe0),
            chrome_active: HexColor::new(0xff, 0xff, 0xff),
            chrome_active_border: HexColor::new(0xb8, 0xc0, 0xcc),
            muted: HexColor::new(0x6b, 0x75, 0x88),
            highlight: HexColor::new(0xc8, 0xd8, 0xea),
            danger: HexColor::new(0xd0, 0x6b, 0x7a),
            search_match: HexColor::new(0xf6, 0xe3, 0xa1),
            search_current: HexColor::new(0xf0, 0xb4, 0x29),
            syntax: SyntaxColors {
                keyword: HexColor::new(0x5e, 0x81, 0xac),
                string: HexColor::new(0x4f, 0x7a, 0x3d),
                comment: HexColor::new(0x8a, 0x94, 0xa6),
                function: HexColor::new(0x2f, 0x6f, 0x8f),
                type_: HexColor::new(0x3b, 0x7a, 0x7a),
                variable: HexColor::new(0x2e, 0x34, 0x40),
                number: HexColor::new(0x8a, 0x5a, 0xa0),
                constant: HexColor::new(0xb0, 0x5f, 0x2b),
                operator: HexColor::new(0x5e, 0x81, 0xac),
                punctuation: HexColor::new(0x6b, 0x75, 0x88),
                attribute: HexColor::new(0xb0, 0x5f, 0x2b),
                property: HexColor::new(0x3b, 0x7a, 0x7a),
                tag: HexColor::new(0xb8, 0x86, 0x1a),
            },
        }
    }

    /// Built-in theme by name.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            FORGE_DARK => Some(Self::forge_dark()),
            FORGE_LIGHT => Some(Self::forge_light()),
            _ => None,
        }
    }

    #[must_use]
    pub fn with_file(mut self, file: &ThemeFile) -> Self {
        let ThemeFile {
            base: _,
            background,
            foreground,
            cursor,
            selection,
            selection_opacity,
            accent,
            chrome,
            chrome_border,
            chrome_active,
            chrome_active_border,
            muted,
            highlight,
            danger,
            search_match,
            search_current,
            syntax,
        } = file;
        set(&mut self.background, *background);
        set(&mut self.foreground, *foreground);
        set(&mut self.cursor, *cursor);
        set(&mut self.selection, *selection);
        set(&mut self.selection_opacity, *selection_opacity);
        set(&mut self.accent, *accent);
        set(&mut self.chrome, *chrome);
        set(&mut self.chrome_border, *chrome_border);
        set(&mut self.chrome_active, *chrome_active);
        set(&mut self.chrome_active_border, *chrome_active_border);
        set(&mut self.muted, *muted);
        set(&mut self.highlight, *highlight);
        set(&mut self.danger, *danger);
        set(&mut self.search_match, *search_match);
        set(&mut self.search_current, *search_current);
        set(&mut self.syntax.keyword, syntax.keyword);
        set(&mut self.syntax.string, syntax.string);
        set(&mut self.syntax.comment, syntax.comment);
        set(&mut self.syntax.function, syntax.function);
        set(&mut self.syntax.type_, syntax.type_);
        set(&mut self.syntax.variable, syntax.variable);
        set(&mut self.syntax.number, syntax.number);
        set(&mut self.syntax.constant, syntax.constant);
        set(&mut self.syntax.operator, syntax.operator);
        set(&mut self.syntax.punctuation, syntax.punctuation);
        set(&mut self.syntax.attribute, syntax.attribute);
        set(&mut self.syntax.property, syntax.property);
        set(&mut self.syntax.tag, syntax.tag);
        self.selection_opacity = self.selection_opacity.clamp(0.0, 1.0);
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, overrides: &ColorOverrides) -> Self {
        set(&mut self.background, overrides.background);
        set(&mut self.foreground, overrides.foreground);
        set(&mut self.cursor, overrides.cursor);
        set(&mut self.selection, overrides.selection);
        set(&mut self.selection_opacity, overrides.selection_opacity);
        set(&mut self.accent, overrides.accent);
        self.selection_opacity = self.selection_opacity.clamp(0.0, 1.0);
        self
    }
}

fn set<T: Copy>(slot: &mut T, value: Option<T>) {
    if let Some(value) = value {
        *slot = value;
    }
}

impl Default for ThemeColors {
    fn default() -> Self {
        Self::forge_dark()
    }
}

/// Where user themes live: `<config dir>/themes/<name>.toml`.
#[must_use]
pub fn themes_dir(config_path: &Path) -> Option<PathBuf> {
    config_path.parent().map(|dir| dir.join("themes"))
}

/// Names of every selectable theme: built-ins first, then user files.
#[must_use]
pub fn available_themes(themes_dir: Option<&Path>) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN_THEMES.iter().map(ToString::to_string).collect();
    if let Some(entries) = themes_dir.and_then(|dir| std::fs::read_dir(dir).ok()) {
        let mut user: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let path = entry.path();
                (path.extension().is_some_and(|ext| ext == "toml"))
                    .then(|| path.file_stem()?.to_str().map(ToString::to_string))
                    .flatten()
            })
            .filter(|name| !names.iter().any(|known| known.eq_ignore_ascii_case(name)))
            .collect();
        user.sort();
        names.extend(user);
    }
    names
}

/// Resolves a theme name to colours. Unknown names fall back to `forge-dark`
/// and are reported so the status line can say so.
///
/// # Errors
///
/// Returns a message when the theme is neither built in nor a readable,
/// valid file; the caller decides whether that is fatal (it never is).
pub fn load_theme(name: &str, themes_dir: Option<&Path>) -> Result<ThemeColors, String> {
    if let Some(builtin) = ThemeColors::builtin(name) {
        return Ok(builtin);
    }
    let Some(dir) = themes_dir else {
        return Err(format!("tema {name:?} desconocido"));
    };
    let path = dir.join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("tema {name:?}: {}: {error}", path.display()))?;
    let file: ThemeFile = toml::from_str(&text)
        .map_err(|error| format!("tema {name:?}: {}: {error}", path.display()))?;
    let base_name = file.base.as_deref().unwrap_or(FORGE_DARK);
    let base = ThemeColors::builtin(base_name)
        .ok_or_else(|| format!("tema {name:?}: base {base_name:?} no es un tema integrado"))?;
    Ok(base.with_file(&file))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_lookup_is_case_insensitive_and_defaults_to_dark() {
        assert_eq!(
            ThemeColors::builtin("Forge-Light"),
            Some(ThemeColors::forge_light())
        );
        assert_eq!(ThemeColors::builtin("nope"), None);
        assert_eq!(ThemeColors::default(), ThemeColors::forge_dark());
    }

    #[test]
    fn theme_files_override_a_base_and_config_overrides_win() {
        let dir = std::env::temp_dir().join(format!("forge-themes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("solar.toml"),
            "base = \"forge-light\"\nbackground = \"#fdf6e3\"\nselection_opacity = 9\n[syntax]\nkeyword = \"#ff0000\"\n",
        )
        .unwrap();
        let theme = load_theme("solar", Some(&dir)).unwrap();
        assert_eq!(theme.background, HexColor::parse("#fdf6e3").unwrap());
        assert_eq!(theme.foreground, ThemeColors::forge_light().foreground);
        assert_eq!(theme.syntax.keyword, HexColor::parse("#ff0000").unwrap());
        assert_eq!(
            theme.syntax.string,
            ThemeColors::forge_light().syntax.string
        );
        assert!((theme.selection_opacity - 1.0).abs() < f32::EPSILON);
        let overrides = ColorOverrides {
            foreground: Some(HexColor::parse("#000000").unwrap()),
            ..ColorOverrides::default()
        };
        assert_eq!(
            theme.with_overrides(&overrides).foreground,
            HexColor::parse("#000000").unwrap()
        );
        let mut names = available_themes(Some(&dir));
        names.retain(|name| name == "solar" || BUILTIN_THEMES.contains(&name.as_str()));
        assert_eq!(names, ["forge-dark", "forge-light", "solar"]);
        assert!(load_theme("missing", Some(&dir)).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
