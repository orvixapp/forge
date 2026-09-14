use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};
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

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AgentRegistryError {
    #[error("agent name cannot be empty")]
    EmptyName,
    #[error("agent {0:?} has no command")]
    EmptyCommand(String),
    #[error("duplicate agent name {0:?}")]
    Duplicate(String),
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
        assert_eq!(
            AgentRegistry::new([agent.clone(), agent]).unwrap_err(),
            AgentRegistryError::Duplicate("codex".into())
        );
    }
}
