//! `PermissionBroker` and capability-based security policy (ARCHITECTURE.md §23.2).
//!
//! Evaluates permission requests for agents, MCP servers and extensions.
//! Decisions can have a TTL of `Once`, `Session` (in-memory) or `Always`
//! (persisted to `.forge/permissions.json`). An audit log records every decision.

use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

/// Target subject governed by a permission policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name")]
pub enum PermissionSubject {
    Extension(String),
    Agent(String),
    McpServer(String),
    Task(String),
    Any,
}

impl PermissionSubject {
    #[must_use]
    pub fn matches_agent(&self, agent_name: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Agent(name) => name.eq_ignore_ascii_case(agent_name),
            _ => false,
        }
    }
}

/// Capability requested by an actor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name")]
pub enum PermissionCapability {
    FsRead,
    FsWrite,
    ProcessSpawn,
    Net,
    Terminal,
    Clipboard,
    Secrets,
    Tool(String),
    Any,
}

impl PermissionCapability {
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, _)
            | (_, Self::Any)
            | (Self::FsRead, Self::FsRead)
            | (Self::FsWrite, Self::FsWrite)
            | (Self::ProcessSpawn, Self::ProcessSpawn)
            | (Self::Net, Self::Net)
            | (Self::Terminal, Self::Terminal)
            | (Self::Clipboard, Self::Clipboard)
            | (Self::Secrets, Self::Secrets) => true,
            (Self::Tool(a), Self::Tool(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
    }

    #[must_use]
    pub fn from_tool_name(tool_name: &str) -> Self {
        if tool_name.starts_with("fs/read") {
            Self::FsRead
        } else if tool_name.starts_with("fs/write") {
            Self::FsWrite
        } else if tool_name.starts_with("terminal/") {
            Self::Terminal
        } else if tool_name == "bash" || tool_name == "exec" || tool_name == "execute_command" {
            Self::ProcessSpawn
        } else {
            Self::Tool(tool_name.to_owned())
        }
    }
}

/// Scope restricting where or how a capability can be exercised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value")]
pub enum PermissionScope {
    Glob(String),
    CommandPattern(String),
    Host(String),
    Path(PathBuf),
    Any,
}

impl PermissionScope {
    #[must_use]
    pub fn matches(&self, target: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Glob(pat) => {
                let trimmed = pat.trim_end_matches('*');
                if trimmed.is_empty() {
                    true
                } else {
                    target.starts_with(trimmed)
                }
            }
            Self::CommandPattern(pat) => {
                let pat_clean = pat.trim().trim_end_matches('*').trim();
                if pat_clean.is_empty() {
                    true
                } else {
                    target.trim().starts_with(pat_clean)
                }
            }
            Self::Host(host) => target.eq_ignore_ascii_case(host),
            Self::Path(path) => {
                let target_path = Path::new(target);
                target_path.starts_with(path) || target_path == path
            }
        }
    }
}

/// Decision reached for a permission query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision {
    Allow,
    Ask,
    Deny,
}

/// Time-to-live of a permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionTtl {
    Once,
    Session,
    Always,
}

/// A configured or remembered policy rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRule {
    pub subject: PermissionSubject,
    pub capability: PermissionCapability,
    pub scope: PermissionScope,
    pub decision: PermissionDecision,
    pub ttl: PermissionTtl,
}

/// Audit log record of a permission request and its outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionAuditEntry {
    pub timestamp_ms: u64,
    pub subject: String,
    pub capability: String,
    pub scope: String,
    pub decision: PermissionDecision,
    pub ttl: PermissionTtl,
}

/// Evaluates and persists security policies for Forge.
#[derive(Debug, Clone)]
pub struct PermissionBroker {
    workspace: PathBuf,
    persistent_rules: Vec<PermissionRule>,
    session_rules: Vec<PermissionRule>,
    audit_log: Vec<PermissionAuditEntry>,
}

impl PermissionBroker {
    #[must_use]
    pub fn new(workspace: PathBuf) -> Self {
        let permissions_file = workspace.join(".forge").join("permissions.json");
        let persistent_rules = if permissions_file.is_file() {
            std::fs::read_to_string(&permissions_file)
                .ok()
                .and_then(|data| serde_json::from_str(&data).ok())
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        Self {
            workspace,
            persistent_rules,
            session_rules: Vec::new(),
            audit_log: Vec::new(),
        }
    }

    /// Evaluates existing policies for `agent_name`, `capability` and `scope`.
    ///
    /// Session decisions have priority over persistent rules. If no rule
    /// matches, returns [`PermissionDecision::Ask`].
    #[must_use]
    pub fn evaluate(
        &self,
        agent_name: &str,
        capability: &PermissionCapability,
        scope: &str,
    ) -> PermissionDecision {
        // Check session rules newest-first
        for rule in self.session_rules.iter().rev() {
            if rule.subject.matches_agent(agent_name)
                && rule.capability.matches(capability)
                && rule.scope.matches(scope)
            {
                return rule.decision;
            }
        }

        // Check persistent rules newest-first
        for rule in self.persistent_rules.iter().rev() {
            if rule.subject.matches_agent(agent_name)
                && rule.capability.matches(capability)
                && rule.scope.matches(scope)
            {
                return rule.decision;
            }
        }

        PermissionDecision::Ask
    }

    /// Records a decision made by the user or policy engine.
    ///
    /// If `ttl` is `Session`, it is retained in memory.
    /// If `ttl` is `Always`, it is retained in memory and written to `.forge/permissions.json`.
    pub fn record_decision(
        &mut self,
        agent_name: &str,
        capability: PermissionCapability,
        scope: PermissionScope,
        decision: PermissionDecision,
        ttl: PermissionTtl,
    ) {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));

        self.audit_log.push(PermissionAuditEntry {
            timestamp_ms: now_ms,
            subject: agent_name.to_owned(),
            capability: format!("{capability:?}"),
            scope: format!("{scope:?}"),
            decision,
            ttl,
        });

        let rule = PermissionRule {
            subject: PermissionSubject::Agent(agent_name.to_owned()),
            capability,
            scope,
            decision,
            ttl,
        };

        match ttl {
            PermissionTtl::Once => {}
            PermissionTtl::Session => {
                self.session_rules.push(rule);
            }
            PermissionTtl::Always => {
                self.session_rules.push(rule.clone());
                self.persistent_rules.push(rule);
                let _ = self.save_persistent();
            }
        }
    }

    /// Saves persistent rules to `<workspace>/.forge/permissions.json`.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be created or the file cannot be written.
    pub fn save_persistent(&self) -> Result<(), std::io::Error> {
        let forge_dir = self.workspace.join(".forge");
        if !forge_dir.exists() {
            std::fs::create_dir_all(&forge_dir)?;
        }
        let permissions_file = forge_dir.join("permissions.json");
        let data = serde_json::to_string_pretty(&self.persistent_rules)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&permissions_file, data)
    }

    #[must_use]
    pub fn session_rules(&self) -> &[PermissionRule] {
        &self.session_rules
    }

    #[must_use]
    pub fn persistent_rules(&self) -> &[PermissionRule] {
        &self.persistent_rules
    }

    #[must_use]
    pub fn audit_log(&self) -> &[PermissionAuditEntry] {
        &self.audit_log
    }
}

/// Picks the best matching ACP `optionId` from an options array according to the user's decision.
#[must_use]
pub fn select_option_id(
    options: &[serde_json::Value],
    decision: PermissionDecision,
    ttl: PermissionTtl,
) -> Option<String> {
    match decision {
        PermissionDecision::Allow => {
            if ttl == PermissionTtl::Always {
                options
                    .iter()
                    .find(|opt| {
                        opt.get("kind")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|k| k.contains("always"))
                    })
                    .or_else(|| options.first())
                    .and_then(extract_option_id)
            } else {
                options
                    .iter()
                    .find(|opt| {
                        opt.get("kind")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|k| k.contains("once") || k.contains("allow"))
                    })
                    .or_else(|| options.first())
                    .and_then(extract_option_id)
            }
        }
        PermissionDecision::Deny => options
            .iter()
            .find(|opt| {
                opt.get("kind")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|k| k.contains("deny") || k.contains("reject"))
            })
            .or_else(|| options.last())
            .and_then(extract_option_id),
        PermissionDecision::Ask => None,
    }
}

fn extract_option_id(option: &serde_json::Value) -> Option<String> {
    option
        .get("optionId")
        .or_else(|| option.get("id"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluates_session_rules_before_persistent() {
        let temp_dir = std::env::temp_dir().join(format!("forge-perm-test-{}", std::process::id()));
        let mut broker = PermissionBroker::new(temp_dir.clone());

        // Default is Ask
        assert_eq!(
            broker.evaluate("OpenCode", &PermissionCapability::Terminal, "printf test"),
            PermissionDecision::Ask
        );

        // Record a session rule: allow printf *
        broker.record_decision(
            "OpenCode",
            PermissionCapability::Terminal,
            PermissionScope::CommandPattern("printf *".into()),
            PermissionDecision::Allow,
            PermissionTtl::Session,
        );

        // Now printf is allowed
        assert_eq!(
            broker.evaluate("OpenCode", &PermissionCapability::Terminal, "printf test"),
            PermissionDecision::Allow
        );

        // Different command still Ask
        assert_eq!(
            broker.evaluate("OpenCode", &PermissionCapability::Terminal, "rm -rf /"),
            PermissionDecision::Ask
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn persists_always_rules_to_disk() {
        let temp_dir =
            std::env::temp_dir().join(format!("forge-perm-persist-{}", std::process::id()));
        let mut broker = PermissionBroker::new(temp_dir.clone());

        broker.record_decision(
            "Codex",
            PermissionCapability::FsWrite,
            PermissionScope::Glob("src/**".into()),
            PermissionDecision::Allow,
            PermissionTtl::Always,
        );

        // Reload broker from same workspace
        let reloaded = PermissionBroker::new(temp_dir.clone());
        assert_eq!(reloaded.persistent_rules().len(), 1);
        assert_eq!(
            reloaded.evaluate("Codex", &PermissionCapability::FsWrite, "src/main.rs"),
            PermissionDecision::Allow
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn selects_correct_option_id() {
        let options = serde_json::json!([
            {"optionId": "allow_once", "name": "Permitir una vez", "kind": "allow_once"},
            {"optionId": "allow_always", "name": "Permitir siempre", "kind": "allow_always"},
            {"optionId": "deny", "name": "Rechazar", "kind": "deny"}
        ]);
        let options_arr = options.as_array().unwrap();

        assert_eq!(
            select_option_id(options_arr, PermissionDecision::Allow, PermissionTtl::Once),
            Some("allow_once".into())
        );
        assert_eq!(
            select_option_id(
                options_arr,
                PermissionDecision::Allow,
                PermissionTtl::Always
            ),
            Some("allow_always".into())
        );
        assert_eq!(
            select_option_id(options_arr, PermissionDecision::Deny, PermissionTtl::Once),
            Some("deny".into())
        );
    }
}
