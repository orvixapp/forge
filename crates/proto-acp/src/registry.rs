use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Command,
};
use thiserror::Error;

/// Declarative description of an ACP adapter. Forge never branches on the
/// agent name: every provider is launched through this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AgentDefinition {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub auth_method: Option<String>,
    pub enabled: bool,
}

impl Default for AgentDefinition {
    fn default() -> Self {
        Self {
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            auth_method: None,
            enabled: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum AgentRegistryError {
    #[error("agent name cannot be empty")]
    EmptyName,
    #[error("agent {0:?} has no command")]
    EmptyCommand(String),
    #[error("duplicate agent name {0:?}")]
    Duplicate(String),
    #[error("cannot read ACP registry {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid ACP registry JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("cannot refresh the official ACP registry: {0}")]
    Refresh(String),
}

pub const OFFICIAL_REGISTRY_URL: &str =
    "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

#[derive(Debug, Clone, Deserialize)]
struct OfficialRegistry {
    agents: Vec<OfficialAgent>,
}

#[derive(Debug, Clone, Deserialize)]
struct OfficialAgent {
    name: String,
    distribution: OfficialDistribution,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct OfficialDistribution {
    #[serde(default)]
    binary: BTreeMap<String, BinaryDistribution>,
    npx: Option<PackageDistribution>,
    uvx: Option<PackageDistribution>,
}

#[derive(Debug, Clone, Deserialize)]
struct BinaryDistribution {
    cmd: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
struct PackageDistribution {
    package: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

/// Stable, ordered registry assembled from user configuration, the ACP
/// registry and PATH detection. Earlier sources win on name collisions.
#[derive(Debug, Clone, Default)]
pub struct AgentRegistry {
    agents: BTreeMap<String, AgentDefinition>,
}

impl AgentRegistry {
    /// Builds a validated registry.
    ///
    /// # Errors
    /// Returns malformed and duplicate definitions instead of silently
    /// launching an unexpected executable.
    pub fn new(
        agents: impl IntoIterator<Item = AgentDefinition>,
    ) -> Result<Self, AgentRegistryError> {
        let mut registry = Self::default();
        for agent in agents {
            registry.register(agent)?;
        }
        Ok(registry)
    }

    /// Registers one definition.
    ///
    /// # Errors
    /// Returns an error for an empty name/command or duplicate name.
    pub fn register(&mut self, agent: AgentDefinition) -> Result<(), AgentRegistryError> {
        if agent.name.trim().is_empty() {
            return Err(AgentRegistryError::EmptyName);
        }
        if agent.command.trim().is_empty() {
            return Err(AgentRegistryError::EmptyCommand(agent.name));
        }
        if self.agents.contains_key(&agent.name) {
            return Err(AgentRegistryError::Duplicate(agent.name));
        }
        self.agents.insert(agent.name.clone(), agent);
        Ok(())
    }

    #[must_use]
    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.agents.get(name).filter(|agent| agent.enabled)
    }

    pub fn available(&self) -> impl Iterator<Item = &AgentDefinition> {
        self.agents.values().filter(|agent| agent.enabled)
    }

    /// Imports the official ACP registry and keeps only adapters whose
    /// executable is already present in `PATH`. Forge never downloads or
    /// installs an adapter implicitly.
    ///
    /// # Errors
    /// Returns malformed registry JSON.
    pub fn import_official_json(&mut self, json: &str) -> Result<usize, AgentRegistryError> {
        let official: OfficialRegistry = serde_json::from_str(json)?;
        let mut imported = 0;
        for agent in official.agents {
            let Some(definition) = installed_definition(agent) else {
                continue;
            };
            if self.agents.contains_key(&definition.name) {
                continue;
            }
            self.register(definition)?;
            imported += 1;
        }
        Ok(imported)
    }

    /// Loads a previously cached official registry.
    ///
    /// # Errors
    /// Returns file-read or JSON errors.
    pub fn import_official_cache(&mut self, path: &Path) -> Result<usize, AgentRegistryError> {
        let json = std::fs::read_to_string(path).map_err(|source| AgentRegistryError::Read {
            path: path.to_owned(),
            source,
        })?;
        self.import_official_json(&json)
    }

    /// Refreshes the official registry cache with `curl`, using a short
    /// timeout. The downloaded document is parsed before replacing the cache.
    ///
    /// # Errors
    /// Returns download, validation, directory, or write failures.
    pub fn refresh_official_cache(path: &Path) -> Result<(), AgentRegistryError> {
        let output = Command::new("curl")
            .args(["-fsSL", "--max-time", "5", OFFICIAL_REGISTRY_URL])
            .output()
            .map_err(|error| AgentRegistryError::Refresh(error.to_string()))?;
        if !output.status.success() {
            return Err(AgentRegistryError::Refresh(format!(
                "curl exited with {}",
                output.status
            )));
        }
        let text = std::str::from_utf8(&output.stdout)
            .map_err(|error| AgentRegistryError::Refresh(error.to_string()))?;
        let _: OfficialRegistry = serde_json::from_str(text)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AgentRegistryError::Read {
                path: parent.to_owned(),
                source,
            })?;
        }
        let temporary = path.with_extension("json.tmp");
        std::fs::write(&temporary, &output.stdout).map_err(|source| AgentRegistryError::Read {
            path: temporary.clone(),
            source,
        })?;
        std::fs::rename(&temporary, path).map_err(|source| AgentRegistryError::Read {
            path: path.to_owned(),
            source,
        })?;
        Ok(())
    }
}

fn installed_definition(agent: OfficialAgent) -> Option<AgentDefinition> {
    let platform = platform_target();
    if let Some(binary) = agent.distribution.binary.get(platform) {
        let command = executable_name(&binary.cmd);
        if executable_on_path(&command) {
            return Some(AgentDefinition {
                name: agent.name,
                command,
                args: binary.args.clone(),
                env: binary.env.clone(),
                ..Default::default()
            });
        }
    }
    for package in [agent.distribution.npx, agent.distribution.uvx]
        .into_iter()
        .flatten()
    {
        let command = package_executable(&package.package);
        if executable_on_path(&command) {
            return Some(AgentDefinition {
                name: agent.name,
                command,
                args: package.args,
                env: package.env,
                ..Default::default()
            });
        }
    }
    None
}

fn platform_target() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "darwin-x86_64",
        ("macos", "aarch64") => "darwin-aarch64",
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        _ => "unsupported",
    }
}

fn executable_name(command: &str) -> String {
    command
        .replace('\\', "/")
        .rsplit('/')
        .next()
        .unwrap_or(command)
        .trim_start_matches("./")
        .to_owned()
}

fn package_executable(package: &str) -> String {
    let without_version = package
        .rfind('@')
        .filter(|index| *index > 0)
        .map_or(package, |index| &package[..index]);
    without_version
        .rsplit('/')
        .next()
        .unwrap_or(without_version)
        .to_owned()
}

fn executable_on_path(command: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|path| {
            let candidate = path.join(command);
            candidate.is_file() || cfg!(windows) && path.join(format!("{command}.exe")).is_file()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_names() {
        let agent = AgentDefinition {
            name: "codex".into(),
            command: "codex-acp".into(),
            ..Default::default()
        };
        assert!(matches!(
            AgentRegistry::new([agent.clone(), agent]),
            Err(AgentRegistryError::Duplicate(name)) if name == "codex"
        ));
    }

    #[test]
    fn package_names_drop_scope_and_version() {
        assert_eq!(
            package_executable("@agentclientprotocol/codex-acp@1.2.3"),
            "codex-acp"
        );
        assert_eq!(package_executable("opencode-ai@2.0.0"), "opencode-ai");
    }

    #[test]
    fn parses_the_official_registry_shape() {
        let mut registry = AgentRegistry::default();
        let count = registry.import_official_json(r#"{"version":"1.0.0","agents":[{"id":"codex-acp","name":"Codex","version":"1.0.0","description":"Codex","distribution":{"npx":{"package":"@agentclientprotocol/codex-acp@1.0.0"}}}]}"#).unwrap();
        assert!(count <= 1);
    }
}
