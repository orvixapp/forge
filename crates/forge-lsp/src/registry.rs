//! Language registry, language definitions and workspace root discovery for LSP.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Configuration of a specific language server executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default)]
    pub initialization_options: Option<Value>,
}

impl ServerConfig {
    #[must_use]
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            command: command.into(),
            args: Vec::new(),
            env: HashMap::new(),
            root_markers: Vec::new(),
            initialization_options: None,
        }
    }

    #[must_use]
    pub fn with_args(mut self, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_root_markers(
        mut self,
        markers: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.root_markers = markers.into_iter().map(Into::into).collect();
        self
    }

    #[must_use]
    pub fn with_init_options(mut self, options: Value) -> Self {
        self.initialization_options = Some(options);
        self
    }
}

/// Language definition mapping file extensions to LSP servers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LanguageDefinition {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub extensions: Vec<String>,
    #[serde(default)]
    pub servers: Vec<ServerConfig>,
}

/// Registry of configured languages and their associated language servers.
#[derive(Debug, Clone)]
pub struct LanguageRegistry {
    languages: HashMap<String, LanguageDefinition>,
    extension_map: HashMap<String, String>,
}

impl Default for LanguageRegistry {
    fn default() -> Self {
        let mut reg = Self {
            languages: HashMap::new(),
            extension_map: HashMap::new(),
        };
        reg.register_defaults();
        reg
    }
}

impl LanguageRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, lang: LanguageDefinition) {
        self.extension_map.retain(|_, id| id != &lang.id);
        for ext in &lang.extensions {
            let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
            self.extension_map.insert(normalized, lang.id.clone());
        }
        self.languages.insert(lang.id.clone(), lang);
    }

    #[must_use]
    pub fn get_language_by_id(&self, id: &str) -> Option<&LanguageDefinition> {
        self.languages.get(id)
    }

    #[must_use]
    pub fn get_language_by_extension(&self, ext: &str) -> Option<&LanguageDefinition> {
        let normalized = ext.trim_start_matches('.').to_ascii_lowercase();
        let lang_id = self.extension_map.get(&normalized)?;
        self.languages.get(lang_id)
    }

    #[must_use]
    pub fn detect_language(&self, path: &Path) -> Option<&LanguageDefinition> {
        let ext = path.extension()?.to_str()?;
        self.get_language_by_extension(ext)
    }

    /// Finds the workspace root for a file based on server root markers.
    #[must_use]
    pub fn find_workspace_root(file_path: &Path, root_markers: &[String]) -> Option<PathBuf> {
        let start_dir = if file_path.is_dir() {
            file_path
        } else {
            file_path.parent()?
        };

        let mut current = start_dir;
        loop {
            for marker in root_markers {
                let candidate = current.join(marker);
                if candidate.exists() {
                    return Some(current.to_path_buf());
                }
            }

            match current.parent() {
                Some(parent) if parent != current => current = parent,
                _ => break,
            }
        }

        None
    }

    fn register_defaults(&mut self) {
        // Rust
        self.register(LanguageDefinition {
            id: "rust".to_string(),
            name: "Rust".to_string(),
            extensions: vec!["rs".to_string()],
            servers: vec![
                ServerConfig::new("rust-analyzer", "rust-analyzer").with_root_markers(vec![
                    "Cargo.toml",
                    "rust-toolchain.toml",
                    ".git",
                ]),
            ],
        });

        // Python
        self.register(LanguageDefinition {
            id: "python".to_string(),
            name: "Python".to_string(),
            extensions: vec!["py".to_string(), "pyi".to_string()],
            servers: vec![
                ServerConfig::new("basedpyright", "basedpyright-langserver")
                    .with_args(["--stdio"])
                    .with_root_markers(vec![
                        "pyproject.toml",
                        "setup.py",
                        "requirements.txt",
                        ".git",
                    ]),
            ],
        });

        // TypeScript
        self.register(LanguageDefinition {
            id: "typescript".to_string(),
            name: "TypeScript".to_string(),
            extensions: vec!["ts".to_string(), "tsx".to_string()],
            servers: vec![
                ServerConfig::new("typescript-language-server", "typescript-language-server")
                    .with_args(["--stdio"])
                    .with_root_markers(vec!["tsconfig.json", "package.json", ".git"]),
            ],
        });

        // JavaScript
        self.register(LanguageDefinition {
            id: "javascript".to_string(),
            name: "JavaScript".to_string(),
            extensions: vec![
                "js".to_string(),
                "jsx".to_string(),
                "mjs".to_string(),
                "cjs".to_string(),
            ],
            servers: vec![
                ServerConfig::new("typescript-language-server", "typescript-language-server")
                    .with_args(["--stdio"])
                    .with_root_markers(vec!["jsconfig.json", "package.json", ".git"]),
            ],
        });

        // Go
        self.register(LanguageDefinition {
            id: "go".to_string(),
            name: "Go".to_string(),
            extensions: vec!["go".to_string()],
            servers: vec![
                ServerConfig::new("gopls", "gopls")
                    .with_root_markers(vec!["go.mod", "go.work", ".git"]),
            ],
        });

        // C / C++
        self.register(LanguageDefinition {
            id: "c".to_string(),
            name: "C".to_string(),
            extensions: vec!["c".to_string(), "h".to_string()],
            servers: vec![
                ServerConfig::new("clangd", "clangd").with_root_markers(vec![
                    "compile_commands.json",
                    "CMakeLists.txt",
                    ".git",
                ]),
            ],
        });

        self.register(LanguageDefinition {
            id: "cpp".to_string(),
            name: "C++".to_string(),
            extensions: vec![
                "cpp".to_string(),
                "cc".to_string(),
                "cxx".to_string(),
                "hpp".to_string(),
                "hxx".to_string(),
            ],
            servers: vec![
                ServerConfig::new("clangd", "clangd").with_root_markers(vec![
                    "compile_commands.json",
                    "CMakeLists.txt",
                    ".git",
                ]),
            ],
        });

        self.register_data_languages();
    }

    fn register_data_languages(&mut self) {
        // TOML
        self.register(LanguageDefinition {
            id: "toml".to_string(),
            name: "TOML".to_string(),
            extensions: vec!["toml".to_string()],
            servers: vec![
                ServerConfig::new("taplo", "taplo")
                    .with_args(["lsp", "stdio"])
                    .with_root_markers(vec![".taplo.toml", "Cargo.toml", ".git"]),
            ],
        });

        // JSON
        self.register(LanguageDefinition {
            id: "json".to_string(),
            name: "JSON".to_string(),
            extensions: vec!["json".to_string(), "jsonc".to_string()],
            servers: vec![
                ServerConfig::new("vscode-json-language-server", "vscode-json-language-server")
                    .with_args(["--stdio"])
                    .with_root_markers(vec!["package.json", ".git"]),
            ],
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_language_detection() {
        let reg = LanguageRegistry::new();
        let rs_file = Path::new("/path/to/src/main.rs");
        let lang = reg.detect_language(rs_file).unwrap();
        assert_eq!(lang.id, "rust");
        assert_eq!(lang.servers[0].command, "rust-analyzer");

        let py_file = Path::new("/path/to/script.py");
        let lang_py = reg.detect_language(py_file).unwrap();
        assert_eq!(lang_py.id, "python");
    }

    #[test]
    fn test_find_workspace_root() {
        let temp = tempfile::tempdir().unwrap();
        let project_dir = temp.path().join("my_rust_project");
        let src_dir = project_dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();

        // Create Cargo.toml in project_dir
        std::fs::write(project_dir.join("Cargo.toml"), "[package]\nname=\"foo\"").unwrap();

        let file = src_dir.join("lib.rs");
        let root = LanguageRegistry::find_workspace_root(&file, &["Cargo.toml".to_string()]);
        assert_eq!(root, Some(project_dir));
    }
}
